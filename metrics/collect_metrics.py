#!/usr/bin/env python3
"""Collect cgroup PSI pressure and runqueue wait metrics into CSV.

- Sampling interval defaults to 1s (best effort, not hard real-time).
- Appends to CSV on every run.
- PSI pressure is computed from `some total=` delta between two samples.
- Runqueue p95 wait is estimated from `/proc/schedstat` deltas.
"""

from __future__ import annotations

import argparse
import csv
import math
import os
import signal
import time
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Dict, Optional, Tuple


DEFAULT_GNURADIO_PSI = "/sys/fs/cgroup/gnuradio.slice/cpu.pressure"
DEFAULT_YAMCS_PSI = "/sys/fs/cgroup/yamcs.slice/cpu.pressure"
DEFAULT_OUTPUT = "metrics/psi_runqueue_metrics.csv"

# p95 for exponential distribution: -ln(1 - 0.95)
P95_FROM_MEAN_FACTOR = -math.log(0.05)


@dataclass
class PsiSample:
    ts_us: int
    total_us: int


@dataclass
class SchedCpuSample:
    wait_ns: int
    nr_slices: int


STOP = False


def _handle_stop(_signum, _frame) -> None:
    global STOP
    STOP = True


def parse_args() -> argparse.Namespace:
    p = argparse.ArgumentParser(description="Collect PSI + runqueue metrics to CSV")
    p.add_argument("--interval", type=float, default=1.0, help="Sampling interval seconds")
    p.add_argument("--output", default=DEFAULT_OUTPUT, help="CSV output path")
    p.add_argument("--gnuradio-psi", default=DEFAULT_GNURADIO_PSI, help="Path to gnuradio cpu.pressure")
    p.add_argument("--yamcs-psi", default=DEFAULT_YAMCS_PSI, help="Path to yamcs cpu.pressure")
    p.add_argument("--max-samples", type=int, default=0, help="0 means run forever")
    return p.parse_args()


def monotonic_us() -> int:
    return time.monotonic_ns() // 1_000


def parse_psi_total_us(path: str) -> int:
    with open(path, "r", encoding="utf-8") as f:
        for line in f:
            line = line.strip()
            if line.startswith("some "):
                for token in line.split():
                    if token.startswith("total="):
                        return int(token.split("=", 1)[1])
                break
    raise ValueError(f"invalid PSI format: {path}")


def read_psi_sample(path: str) -> Optional[PsiSample]:
    try:
        total_us = parse_psi_total_us(path)
        return PsiSample(ts_us=monotonic_us(), total_us=total_us)
    except (OSError, ValueError):
        return None


def psi_pressure(prev: Optional[PsiSample], cur: Optional[PsiSample]) -> float:
    if prev is None or cur is None:
        return float("nan")
    dt = cur.ts_us - prev.ts_us
    if dt <= 0:
        return float("nan")
    dtotal = cur.total_us - prev.total_us
    if dtotal < 0:
        return float("nan")
    return max(0.0, min(1.0, dtotal / dt))


def read_schedstat() -> Dict[int, SchedCpuSample]:
    out: Dict[int, SchedCpuSample] = {}
    with open("/proc/schedstat", "r", encoding="utf-8") as f:
        for line in f:
            line = line.strip()
            if not line.startswith("cpu"):
                continue
            parts = line.split()
            if len(parts) < 10:
                continue
            cpu_tag = parts[0]
            if cpu_tag == "cpu":
                continue
            cpu_id_str = cpu_tag[3:]
            if not cpu_id_str.isdigit():
                continue
            cpu_id = int(cpu_id_str)
            # Linux schedstat v17 exposes final fields as:
            # ... running_time_ns wait_time_ns timeslices
            wait_ns = int(parts[-2])
            nr_slices = int(parts[-1])
            out[cpu_id] = SchedCpuSample(wait_ns=wait_ns, nr_slices=nr_slices)
    return out


def estimate_runqueue_p95_wait_us_mean(
    prev: Optional[Dict[int, SchedCpuSample]],
    cur: Dict[int, SchedCpuSample],
) -> float:
    if prev is None:
        return float("nan")

    per_cpu_p95_us = []
    for cpu_id, cur_s in cur.items():
        prev_s = prev.get(cpu_id)
        if prev_s is None:
            continue
        d_wait_ns = cur_s.wait_ns - prev_s.wait_ns
        d_slices = cur_s.nr_slices - prev_s.nr_slices
        if d_wait_ns < 0 or d_slices <= 0:
            continue

        mean_wait_us = (d_wait_ns / d_slices) / 1_000.0
        p95_wait_us_est = mean_wait_us * P95_FROM_MEAN_FACTOR
        per_cpu_p95_us.append(p95_wait_us_est)

    if not per_cpu_p95_us:
        return float("nan")
    return sum(per_cpu_p95_us) / len(per_cpu_p95_us)


def ensure_csv_header(path: Path) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    if path.exists() and path.stat().st_size > 0:
        return
    with open(path, "a", newline="", encoding="utf-8") as f:
        writer = csv.writer(f)
        writer.writerow(
            [
                "ts_unix_s",
                "ts_iso8601",
                "gnuradio_psi_pressure",
                "yamcs_psi_pressure",
                "runqueue_p95_wait_us_mean",
            ]
        )


def append_row(path: Path, row: Tuple[object, ...]) -> None:
    with open(path, "a", newline="", encoding="utf-8") as f:
        csv.writer(f).writerow(row)


def fmt_float(v: float) -> str:
    if math.isnan(v):
        return ""
    return f"{v:.6f}"


def main() -> int:
    args = parse_args()
    if args.interval <= 0:
        raise SystemExit("--interval must be > 0")

    signal.signal(signal.SIGINT, _handle_stop)
    signal.signal(signal.SIGTERM, _handle_stop)

    out_path = Path(args.output)
    ensure_csv_header(out_path)

    prev_gnuradio = read_psi_sample(args.gnuradio_psi)
    prev_yamcs = read_psi_sample(args.yamcs_psi)
    prev_sched = read_schedstat()

    samples = 0
    while not STOP and (args.max_samples == 0 or samples < args.max_samples):
        time.sleep(args.interval)

        now = datetime.now(timezone.utc)
        ts_unix_s = now.timestamp()
        ts_iso = now.isoformat()

        cur_gnuradio = read_psi_sample(args.gnuradio_psi)
        cur_yamcs = read_psi_sample(args.yamcs_psi)
        cur_sched = read_schedstat()

        gnuradio_pressure = psi_pressure(prev_gnuradio, cur_gnuradio)
        yamcs_pressure = psi_pressure(prev_yamcs, cur_yamcs)
        runqueue_p95_wait_us_mean = estimate_runqueue_p95_wait_us_mean(prev_sched, cur_sched)

        append_row(
            out_path,
            (
                f"{ts_unix_s:.3f}",
                ts_iso,
                fmt_float(gnuradio_pressure),
                fmt_float(yamcs_pressure),
                fmt_float(runqueue_p95_wait_us_mean),
            ),
        )

        prev_gnuradio = cur_gnuradio
        prev_yamcs = cur_yamcs
        prev_sched = cur_sched
        samples += 1

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
