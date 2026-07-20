"""Unit tests for the multi-cursor iolog generator.

These pin the generator's offset sequence to the invariants of run_interleaved_cursors in the
s3io_benchmark example: contiguous regions cover the object exactly once, overlapping regions
over-read, every read is within its region, and the reads round-robin across regions.
"""

import importlib.util
import io
from pathlib import Path

_MODULE_PATH = Path(__file__).resolve().parent.parent / "fio" / "gen_interleaved_iolog.py"
_spec = importlib.util.spec_from_file_location("gen_interleaved_iolog", _MODULE_PATH)
gen = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(gen)


class TestBuildRegions:
    def test_multi_cursor_regions_partition_object(self):
        size = 128 * 1024 * 1024
        regions = gen.build_regions("multi_cursor", size, 8)

        assert len(regions) == 8
        # Contiguous and gapless: each region starts where the previous ended.
        assert regions[0][0] == 0
        for (_, prev_end), (next_start, _) in zip(regions, regions[1:]):
            assert next_start == prev_end
        # The last region reaches EOF, absorbing any remainder.
        assert regions[-1][1] == size

    def test_multi_cursor_last_region_absorbs_remainder(self):
        # 100 is not divisible by 8, so the last region must be larger.
        regions = gen.build_regions("multi_cursor", 100, 8)
        assert regions[-1][1] == 100
        assert regions[-1][1] - regions[-1][0] > regions[0][1] - regions[0][0]

    def test_overlapping_regions_all_reach_eof(self):
        size = 2 * 1024 * 1024 * 1024
        regions = gen.build_regions("overlapping_cursors", size, 4)

        assert len(regions) == 4
        spacing = size // 4
        assert [start for start, _ in regions] == [0, spacing, 2 * spacing, 3 * spacing]
        assert all(end == size for _, end in regions)


class TestBuildReads:
    def test_multi_cursor_total_equals_object_size(self):
        size = 64 * 1024 * 1024
        regions = gen.build_regions("multi_cursor", size, 8)
        reads = gen.build_reads(regions, visit=128 * 1024, read_size=64 * 1024)

        # Contiguous partition read exactly once => total bytes == object size.
        assert sum(length for _, length in reads) == size

    def test_overlapping_total_exceeds_object_size(self):
        size = 64 * 1024 * 1024
        regions = gen.build_regions("overlapping_cursors", size, 8)
        reads = gen.build_reads(regions, visit=128 * 1024, read_size=64 * 1024)

        # Every cursor runs to EOF, so overlap makes the total strictly larger than the object.
        assert sum(length for _, length in reads) > size

    def test_every_read_lies_within_a_region(self):
        size = 64 * 1024 * 1024
        regions = gen.build_regions("multi_cursor", size, 8)
        reads = gen.build_reads(regions, visit=128 * 1024, read_size=64 * 1024)

        for offset, length in reads:
            assert any(start <= offset and offset + length <= end for start, end in regions)

    def test_reads_are_interleaved_across_regions(self):
        # The generator visits every region in each round (reading `visit` bytes from a region in
        # `read_size` blocks before advancing), rather than draining region 0 first. So each round
        # emits visit/read_size reads per region, cycling through all regions.
        size = 8 * 1024 * 1024
        num_cursors = 4
        visit = 128 * 1024
        read_size = 64 * 1024
        regions = gen.build_regions("multi_cursor", size, num_cursors)
        reads = gen.build_reads(regions, visit=visit, read_size=read_size)

        region_size = size // num_cursors
        reads_per_visit = visit // read_size  # blocks read from a region before advancing
        # First round: reads_per_visit reads from region 0, then region 1, etc. The first read of
        # each region lands at that region's start, at index i * reads_per_visit.
        for i in range(num_cursors):
            assert reads[i * reads_per_visit][0] == i * region_size
        # Region 1's first read must appear long before region 0 is drained (which takes
        # region_size / read_size reads), proving interleaving rather than sequential draining.
        assert region_size // read_size > reads_per_visit

    def test_read_size_blocks_do_not_exceed_read_size(self):
        size = 8 * 1024 * 1024
        regions = gen.build_regions("multi_cursor", size, 4)
        read_size = 64 * 1024
        reads = gen.build_reads(regions, visit=128 * 1024, read_size=read_size)

        assert all(length <= read_size for _, length in reads)

    def test_multi_cursor_exact_sequence(self):
        # Fully enumerable case: size=8, 2 cursors -> regions [0,4),[4,8); visit=4, read_size=2.
        # Round 1: region 0 reads (0,2),(2,2) [visit exhausted], region 1 reads (4,2),(6,2).
        # Both regions are now exhausted, so that is the entire sequence. This pins the exact
        # round-robin order and block striding, not just aggregate properties.
        regions = gen.build_regions("multi_cursor", 8, 2)
        reads = gen.build_reads(regions, visit=4, read_size=2)
        assert reads == [(0, 2), (2, 2), (4, 2), (6, 2)]

    def test_overlapping_exact_sequence(self):
        # size=8, 2 cursors -> spacing 4, regions [0,8),[4,8); visit=4, read_size=2.
        # Round 1: region 0 (0,2),(2,2); region 1 (4,2),(6,2) [reaches EOF, done].
        # Round 2: region 0 (4,2),(6,2) [reaches EOF, done]. Region 1's tail is re-read by region 0,
        # which is exactly the overlap. Pins the overlapping read order and the over-read.
        regions = gen.build_regions("overlapping_cursors", 8, 2)
        reads = gen.build_reads(regions, visit=4, read_size=2)
        assert reads == [(0, 2), (2, 2), (4, 2), (6, 2), (4, 2), (6, 2)]


class TestWriteIolog:
    def test_emits_valid_fio_v2_iolog(self):
        # The whole point of the generator is emitting valid fio version-2 iolog syntax, so pin the
        # exact text: the version header, add/open/close lifecycle opcodes, and `read <off> <len>`
        # lines in order. A typo in any opcode would otherwise pass every offset-arithmetic test.
        out = io.StringIO()
        gen.write_iolog(out, "obj.bin", [(0, 2), (2, 2)])
        assert out.getvalue() == (
            "fio version 2 iolog\n"
            "obj.bin add\n"
            "obj.bin open\n"
            "obj.bin read 0 2\n"
            "obj.bin read 2 2\n"
            "obj.bin close\n"
        )

    def test_no_reads_still_emits_lifecycle(self):
        out = io.StringIO()
        gen.write_iolog(out, "obj.bin", [])
        assert out.getvalue() == "fio version 2 iolog\nobj.bin add\nobj.bin open\nobj.bin close\n"
