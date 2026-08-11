import logging
import os
import subprocess
import sys
import tempfile
from datetime import datetime, timezone
from typing import Any

import hydra
from benchmarks.base_benchmark import BaseBenchmark
from benchmarks.command import Command, CommandResult
from benchmarks.mountpoint import cleanup_mp, mount_mp
from omegaconf import DictConfig

log = logging.getLogger(__name__)


class FioBenchmark(BaseBenchmark):
    def __init__(self, cfg: DictConfig, metadata: dict[str, Any]):
        self.cfg = cfg
        self.metadata = metadata  # Use the metadata passed from benchmark.py
        self.mount_dir = None
        self.fio_output_filepath = None

    def _get_dev_id(self):
        with open('/proc/self/mountinfo', 'r') as f:
            for line in f:
                fields = line.split()
                # Use mounted dir to extract the dev id
                if fields[4] == self.mount_dir:
                    dev_id = fields[2]
                    return dev_id
        raise RuntimeError(f"Could not find device ID for mount point {self.mount_dir}")

    def _set_read_ahead(self, bytes):
        dev_id = self._get_dev_id()
        read_ahead_path = f"/sys/class/bdi/{dev_id}/read_ahead_kb"
        bytes_in_kb = bytes // 1024
        cmd = f'echo {bytes_in_kb} > {read_ahead_path}'
        subprocess.run(['sudo', 'sh', '-c', cmd], check=True, capture_output=True)
        log.info(f"Set read_ahead_kb to {bytes} for device {dev_id}")

    def setup(self, with_flamegraph: bool = False) -> dict[str, Any]:
        self.mount_dir = tempfile.mkdtemp(suffix=".mountpoint-s3")
        mount_metadata = mount_mp(self.cfg, self.mount_dir, with_flamegraph)
        self.metadata.update(mount_metadata)
        return self.metadata

    def get_command(self) -> Command:
        FIO_BINARY = "fio"
        fio_job_name = self.cfg.benchmarks.fio.fio_benchmark
        fio_job_filepath = hydra.utils.to_absolute_path(f"fio/{fio_job_name}.fio")
        self.fio_output_filepath = f"fio.{fio_job_name}.json"

        common_args = [
            FIO_BINARY,
            "--eta=never",
            "--output-format=json",
            f"--output={self.fio_output_filepath}",
        ]

        # The multi_cursor_read job replays a generated iolog through a single fd rather than
        # laying out and reading whole files. It deliberately omits --directory: fio 3.32 segfaults
        # replaying a version-2 iolog with a relative filename against --directory, so the file is
        # located by an absolute replay_redirect target instead (see _multi_cursor_env).
        if fio_job_name == "multi_cursor_read":
            subprocess_args = [*common_args, fio_job_filepath]
            return Command(args=subprocess_args, env=self._multi_cursor_env())

        subprocess_args = [*common_args, f"--directory={self.mount_dir}", fio_job_filepath]

        fio_env = {}
        fio_env["APP_WORKERS"] = str(self.cfg.application_workers)
        fio_env["SIZE_GIB"] = str(self.cfg.object_size_in_gib)
        fio_env["DIRECT"] = "1" if self.cfg.benchmarks.fio.direct_io else "0"
        fio_env["UNIQUE_DIR"] = datetime.now(tz=timezone.utc).isoformat()
        fio_env["IO_ENGINE"] = self.cfg.benchmarks.fio.fio_io_engine
        fio_env["RUN_TIME"] = str(self.cfg.run_time)
        fio_env["BLOCK_SIZE"] = str(self.cfg.read_size)

        # Increase the read_ahead_kb limit to allow reads higher than 256K.
        # The script needs sudo permissions to overwrite this limit
        if not self.cfg.benchmarks.fio.direct_io and self.cfg.read_size > 256 * 1024:
            self._set_read_ahead(self.cfg.read_size)

        log.info("FIO command prepared with args: %s; env: %s", subprocess_args, fio_env)

        return Command(args=subprocess_args, env=fio_env)

    def _multi_cursor_env(self) -> dict[str, str]:
        """Generate the interleaved iolog for the multi_cursor_read job and return its fio env.

        The read sequence reproduces run_interleaved_cursors from the s3io_benchmark example. The
        number of interleaved streams is the *workload* parameter benchmarks.fio.multi_cursor.
        num_cursors and is deliberately independent of the mount's --max-cursors: an A/B that varies
        only mountpoint.max_cursors must replay the identical workload, so that the single-cursor
        arm runs the same interleaved reads (served by one thrashing cursor) rather than a different,
        sequential workload. The iolog is written into the Hydra job directory (cwd).
        """
        mc = self.cfg.benchmarks.fio.multi_cursor
        num_cursors = mc.num_cursors

        # The mount cap must be set explicitly: without it Mountpoint uses its built-in default,
        # which would silently run the interleaved workload in some unintended cursor mode and
        # invalidate the measurement. This is the workload's num_cursors' independent counterpart.
        if self.cfg.mountpoint.max_cursors is None:
            raise ValueError("multi_cursor_read requires mountpoint.max_cursors to be set explicitly")

        iolog_path = os.path.abspath("multi_cursor.iolog")
        generator = hydra.utils.to_absolute_path("fio/gen_interleaved_iolog.py")
        gen_args = [
            sys.executable,
            generator,
            f"--object-name={mc.object_key}",
            f"--object-size={mc.object_size_bytes}",
            f"--num-cursors={num_cursors}",
            f"--visit={mc.visit}",
            f"--read-size={mc.read_size}",
            f"--pattern={mc.pattern}",
            f"--output={iolog_path}",
        ]
        log.info("Generating multi-cursor iolog: %s", gen_args)
        subprocess.run(gen_args, check=True)

        # replay_redirect points every replayed read at the mounted file by absolute path. This
        # avoids fio 3.32's segfault on relative-filename version-2 iolog replay under --directory
        # and keeps the iolog itself mount-agnostic. DIRECT honours benchmarks.fio.direct_io (as the
        # other jobs do) so the replayed offsets/sizes reach FUSE without kernel readahead reshaping
        # them; without it the multi_cursor job would silently force buffered mode.
        replay_target = os.path.join(self.mount_dir, mc.object_key)
        return {
            "IOLOG": iolog_path,
            "REPLAY_TARGET": replay_target,
            "DIRECT": "1" if self.cfg.benchmarks.fio.direct_io else "0",
            # Wall-clock cap, same run_time knob the other jobs use. read_iolog does not loop, so
            # this only bounds a slow (single-cursor) arm; a fast arm finishes early and stops.
            "RUN_TIME": str(self.cfg.run_time),
        }

    def post_process(self, result: CommandResult) -> dict[str, Any]:
        cleanup_mp(self.mount_dir)
        if result.returncode != 0:
            log.error(f"FIO process failed with exit code {result.returncode}")
            raise subprocess.CalledProcessError(result.returncode, ["fio"])

        self.metadata["fio_output_file"] = self.fio_output_filepath
        return self.metadata
