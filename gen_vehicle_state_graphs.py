import math
from pathlib import Path
import csv
from dataclasses import dataclass
from datetime import datetime, timedelta
from typing import TypeAlias
import matplotlib.pyplot as plt
import numpy as np
from matplotlib.collections import LineCollection
import matplotlib.dates as mdates

output_tl = Path("output/pos-deltas")
otl_users = list(output_tl.glob("*"))

print(f"otl_users_len {len(otl_users)}")

user_csvs: list[(Path, list[Path])] = []

for user_tl_folder in otl_users:
    # print(f"Processing: {user_tl_folder}")
    csvs = list(user_tl_folder.glob("*.csv"))
    # print(f"nb csvs: {len(csvs)}")
    user_csvs.append((user_tl_folder, csvs))

Vec3: TypeAlias = tuple[float, float, float]


def vec3_from_str(val: str) -> np.ndarray:
    """sample in: "[1.4314727783203125|0.00409698486328125|0.08428955078125]" """
    if val[0] != "[" or val[-1] != "]":
        raise ValueError(
            f"value should be in the format `[f64, f64, f64]`, but got: `{val}`"
        )
    val = val[1:-1]
    x, y, z = [float(v.strip()) for v in val.split("|")]
    return np.array([x, y, z])


@dataclass
class VehicleStateRow:
    ROWS = ["ts", "dt", "dist3", "dist", "pos0", "pos1"]
    ts: datetime
    dt: float
    dist3: np.ndarray
    dist: float
    pos0: np.ndarray
    pos1: np.ndarray

    @classmethod
    def from_row(cls, row: list[str]) -> "VehicleStateRow":
        row = [e.strip() for e in row]
        # print(f"from_row: {row}")
        return cls(
            datetime.fromtimestamp(int(row[0])),
            float(row[1]),
            vec3_from_str(row[2]),
            float(row[3]),
            vec3_from_str(row[4]),
            vec3_from_str(row[5]),
        )

    def get_col(self) -> str:
        return ""


import itertools

ignore_before = datetime.fromtimestamp(1715126458)

user_csvs: list[Path] = sorted(user_csvs, key=lambda p: p[0].stem)

top3_names = {"BrenTM", "eLconn21", "Hazardu"}
top3_paths = [uc for uc in user_csvs if uc[0].stem in top3_names]
top3_names.add("Hazardu.")
user_csvs = top3_paths + [uc for uc in user_csvs if uc[0].stem not in top3_names]
print(f"user_csvs[:4]: {user_csvs[:4]}")

for tl_folder, csvs in user_csvs:
    csvs: list[Path] = csvs
    csvs = sorted(csvs, key=lambda p: p.stem)
    tl_folder: Path = tl_folder
    user_name = tl_folder.stem
    print(f"user: {user_name}")
    for i, csv_path in enumerate(csvs):
        p: Path = csv_path
        reader = csv.reader(p.read_text().splitlines())
        headings = reader.__next__()
        rows: list[VehicleStateRow] = [
            VehicleStateRow.from_row(line) for line in reader
        ]
        if len(rows) == 0:
            print(f"Warning: no rows: {p}")
            continue
        # timestamp
        ts = []
        # delta heights
        dhs_p = []
        dhs_n = []
        # heights
        hs = []
        # delta positions (magnitude)
        dp = []

        map_ids = []
        has_dd2 = False
        for row in rows:
            ts.append(row.ts)
            h = row.dist3[1]
            dhs_p.append(h / row.dt if h > 0 else 0)
            dhs_n.append(h / row.dt if h < 0 else 0)
            # hs.append((row.pos0[1] + row.pos1[1]) * 0.5)
            hs.append(row.pos0[1])
            dp.append(row.dist / row.dt)
            if dhs_p[-1] > 50.0:
                print(f"... a delta height > 50: {dhs_p[-1]}")

        if max(dhs_p) > 50.0:
            print(f"max delta height > 50: {max(dhs_p)}")

        start: datetime = min(ts)
        end: datetime = max(ts)

        dur = end - start
        if dur.total_seconds() < 2.0:
            # print(f"Session that lasted less than 2s?")
            continue

        # 2024-05-08T00:00:58.000Z
        if start < ignore_before:
            continue

        ts = mdates.date2num(ts)
        min_ts = min(ts)
        max_ts = max(ts)

        # points = np.array([ts, dhs]).T.reshape(-1, 1, 2)
        # print(f"P = {points}")
        # segments = np.concatenate([points[:-1], points[1:]], axis=1)
        # print(f"S = {segments}")
        t_dhs_p = list(zip(ts, dhs_p))
        t_dhs_n = list(zip(ts, dhs_n))
        t_hs = list(zip(ts, hs))
        t_dp = list(zip(ts, dp))

        # ts = np.array(ts)

        xs = np.linspace(min_ts, max_ts, int(math.ceil(dur.total_seconds() / 5)))
        # create ys for dhs_p and dhs_n in the corresponding slot only, otherwise 0
        y_dhs_p = np.zeros(len(xs))
        y_dhs_n = np.zeros(len(xs))
        min_j = 0
        min_k = 0
        for j, x in enumerate(xs):
            if j < min_j:
                continue
            for k, (t, dh) in enumerate(t_dhs_p):
                if k < min_k:
                    continue
                if t < x:
                    y_dhs_p[j] = dh
                else:
                    # min_k = k
                    break
            for k, (t, dh) in enumerate(t_dhs_n):
                if k < min_k:
                    continue
                if t < x:
                    y_dhs_n[j] = dh
                else:
                    min_k = k
                    break
            min_j = j

        # lc = LineCollection(
        #     [t_dhs], cmap="viridis", array=[0]
        # )  # , norm=plt.Normalize(0, 10))

        fig, ax = plt.subplots(figsize=(20, 8))
        # ax.add_collection(lc)

        ax.bar(xs, y_dhs_p, width=0.0001, color="green", alpha=0.5)
        ax.bar(xs, y_dhs_n, width=0.0001, color="red", alpha=0.5)
        # ax.set_ylim(min(dhs_n), max(dhs_p))

        ax.autoscale()
        ax.set_xbound(None, None)
        # ax.set_ybound(-200, 500)
        ax.set_ylabel("Delta Height (m/s) between updates")
        ax.xaxis.set_major_locator(mdates.MinuteLocator(interval=5))
        ax.xaxis.set_major_formatter(mdates.DateFormatter("%Y-%m-%d %H:%M:%S %Z"))
        plt.xticks(rotation=45, ha="right")
        title_text = plt.title(
            f"Pos Deltas: {user_name} ({dur.total_seconds() / 60.:.1f} min) -- {start.isoformat()} to {end.isoformat()}",
            fontdict={"fontsize": 10},
        )

        lc2 = LineCollection(
            [t_hs], cmap="viridis", colors=["red"]
        )  # , norm=plt.Normalize(0, 10))
        y2ax = ax.twinx()
        y2ax.add_collection(lc2)
        y2ax.autoscale()
        # y2ax.set_ybound(None, None)
        # y2ax.set_xbound(None, None)
        y2ax.set_ylabel("Height (m)")

        plt.tight_layout()

        out_f = p.parent / f"{p.stem}.png"
        plt.savefig(out_f)
        plt.close()
        print(f"saved: {out_f}")

        # if i > 3: break
        # break
    # break
