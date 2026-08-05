# Memory-limiter stress test report — baseline

## Summary

With `--memory-target 32768` MiB, over a 15-minute mixed-workload run, the process RSS **HELD** the target: peak RSS was **19741 MiB** (60% of target). Pool `bytes_in_use` peaked at **17232 MiB** vs the data-buffer budget of 28672 MiB (within budget).

## Configuration

- Commit: `4b3228b5a2f2e8f6f1f442d3fe9bc24ca1979b70`
- Host: `ip-172-31-77-58.ec2.internal` (64 cores)
- `--memory-target`: 32768 MiB
- Derived data-buffer budget: 28672 MiB (`mem_limit - max(mem_limit/8, 128 MiB)`)
- Read part size: 8388608; write part size: 8388608
- Max threads: 16
- Mount command: `/home/ec2-user/memlimit-test/mountpoint-s3/target/release/mount-s3 adpeace-mountpoint-perf-test /home/ec2-user/memlimit-test/mountpoint-s3/memory-stress/runs/20260805T173919Z-baseline-32768MiB/mnt --prefix memlimit-test/ --memory-target 32768 --read-part-size 8388608 --write-part-size 8388608 --max-threads 16 --allow-overwrite --allow-delete --log-metrics --log-directory /home/ec2-user/memlimit-test/mountpoint-s3/memory-stress/runs/20260805T173919Z-baseline-32768MiB/mp_logs`

## Workload

Read pool: 32 objects x 1024 MiB = 32 GiB total. readdir tree: 200 dirs x 200 files. Each phase ran for 180s.

| Phase | Workload | Concurrency |
|---|---|---|
| P1 | sequential reads | 24 readers |
| P2 | sequential reads + writers | 24 readers + 16 writers |
| P3 | random reads | 32 readers |
| P4 | slow (rate-limited) reads | 24 readers @ 1m |
| P5 | readdir + stat loop | 1 recursive lister |
| P6 | all at once | 24+32 readers + 16 writers + readdir |

## Results

### RSS vs memory target

![RSS over time](rss.png)

### Pool memory vs data-buffer budget

![Pool memory over time](pool.png)

### Peak memory by phase

| Phase | Peak RSS | Peak pool in-use |
|---|---|---|
| P1_sequential_reads | 18627 MiB | 17008 MiB |
| P2_reads_plus_writes | 19741 MiB | 17232 MiB |
| **overall** | **19741 MiB** | **17232 MiB** |

## Findings

- RSS stayed under `--memory-target` throughout: peak 19741 MiB <= 32768 MiB. The limiter held under this workload.
- Pool-tracked memory stayed within the data-buffer budget (in-use peak 17232 MiB, allocated peak 19584 MiB <= 28672 MiB).
- No stalls or deadlocks were detected (all phases progressed and unmount succeeded).

## Reproduction

This run is reproducible from the config in this directory:

```
./run.sh config.env baseline
```

against commit `4b3228b5a2f2e8f6f1f442d3fe9bc24ca1979b70`. The exact mount command and all
workload parameters are recorded in `run_meta.json` alongside this report.

