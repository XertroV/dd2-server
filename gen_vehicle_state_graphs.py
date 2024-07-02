import math
from pathlib import Path
import csv
from dataclasses import dataclass
from datetime import datetime, timedelta
import sys
from typing import TypeAlias
import matplotlib.pyplot as plt
import numpy as np
from matplotlib.collections import LineCollection
import matplotlib.dates as mdates

output_tl = Path("output/pos-deltas")
map_path = Path("output/maps")
otl_users = list(output_tl.glob("*"))

maps_users = map_path.glob("*")

print(f"otl_users_len {len(otl_users)}")

user_csvs: list[(Path, list[Path])] = []
user_maps_csvs: list[(Path, list[Path])] = []

for fldr in otl_users:
    # print(f"Processing: {user_tl_folder}")
    csvs = list(fldr.glob("*vehicle-states.csv"))
    # print(f"nb csvs: {len(csvs)}")
    user_csvs.append((fldr, csvs))

for fldr in maps_users:
    # print(f"Processing: {user_tl_folder}")
    csvs = list(fldr.glob("*all-maps.csv"))
    # print(f"nb csvs: {len(csvs)}")
    user_maps_csvs.append((fldr, csvs))


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


@dataclass
class MapRow:
    ROWS = [
        "ctx_ix",
        "start_ts",
        "map_name",
        "bi_count",
        "since_init_s",
        "end_ts",
        "duration_s",
    ]
    ctx_ix: int
    start_ts: datetime
    map_name: str
    bi_count: int
    since_init_s: float
    end_ts: datetime
    duration_s: float

    @classmethod
    def from_row(cls, row: list[str]) -> "MapRow":
        row = [e.strip() for e in row]
        return cls(
            int(row[0]),
            datetime.fromtimestamp(int(row[1])),
            row[2],
            int(row[3]),
            float(row[4]),
            datetime.fromtimestamp(int(row[5])),
            float(row[6]),
        )

    def copy(self) -> "MapRow":
        return MapRow(
            self.ctx_ix,
            self.start_ts,
            self.map_name,
            self.bi_count,
            self.since_init_s,
            self.end_ts,
            self.duration_s,
        )


class MapLookup:
    def __init__(self, map_csv: Path):
        self.map_csv = map_csv
        self.map_data: list[MapRow] = []
        self.load_map_data()
        self.headings = []

    def load_map_data(self):
        reader = csv.reader(self.map_csv.read_text().splitlines())
        self.headings = reader.__next__()
        for row in reader:
            self.map_data.append(MapRow.from_row(row))

    def get_map(self, ts: datetime) -> MapRow:
        l = 0
        r = len(self.map_data) - 1
        while l < r:
            m = (l + r) // 2
            if self.map_data[m].start_ts <= ts <= self.map_data[m].end_ts:
                return self.map_data[m]
            if ts > self.map_data[m].end_ts:
                l = m + 1
            elif ts < self.map_data[m].start_ts:
                r = m - 1
            else:
                r = m
        return self.map_data[l]


def build_maps_lookup(map_csv: Path) -> MapLookup:
    return MapLookup(map_csv)


import itertools

ignore_before = datetime.fromtimestamp(1715126458)

user_csvs: list[Path] = sorted(user_csvs, key=lambda p: p[0].stem)
user_maps_csvs: list[Path] = sorted(user_maps_csvs, key=lambda p: p[0].stem)

top3_names = {"BrenTM", "eLconn21", "Hazardu"}
top3_paths = [uc for uc in user_csvs if uc[0].stem in top3_names]
top3_m_paths = [uc for uc in user_maps_csvs if uc[0].stem in top3_names]
top3_names.add("Hazardu.")
user_csvs = top3_paths + [uc for uc in user_csvs if uc[0].stem not in top3_names]
user_maps_csvs = top3_m_paths + [
    uc for uc in user_maps_csvs if uc[0].stem not in top3_names
]
# print(f"user_csvs[:4]: {user_csvs[:4]}")


for (tl_folder, csvs), (map_folder, map_csvs) in zip(user_csvs, user_maps_csvs):
    csv_p: Path = csvs[0]
    map_csv_p: Path = map_csvs[0]
    tl_folder: Path = tl_folder
    user_name = tl_folder.stem
    print(f"user: {user_name}")

    page_len_s = 86400 // 4

    map_lookup: MapLookup = build_maps_lookup(map_csv_p)

    pages: list[tuple[datetime, list[VehicleStateRow]]] = []
    last_page = -1
    reader = csv.reader(csv_p.read_text().splitlines())
    headings = reader.__next__()
    rows: list[VehicleStateRow] = [VehicleStateRow.from_row(line) for line in reader]
    for r in rows:
        pg = r.ts.timestamp() // page_len_s
        if pg != last_page:
            pg_start_sec = int(r.ts.timestamp()) % page_len_s
            pages.append((r.ts - timedelta(seconds=pg_start_sec), []))
            last_page = pg
        pages[-1][1].append(r)

    print(f"pages: {len(pages)}")

    fig_width = 20.0

    for i, (start_ts, page) in enumerate(pages):
        end_ts = start_ts + timedelta(seconds=page_len_s)
        print(f"page: {i} -- {start_ts} -> {end_ts}")
        if len(page) == 0:
            print(f"Warning: no rows: {i}")
            continue

        print(f"starting page: {i} -- {len(page)}")

        # timestamp
        ts = []
        # delta heights
        dhs_p = []
        dhs_n = []
        # heights
        hs = []
        # delta positions (magnitude)
        dp = []

        page_map_rows: list[MapRow] = []
        map_row = map_lookup.get_map(page[0].ts).copy()
        map_row.start_ts = max(map_row.start_ts, start_ts)
        max_end_ts = start_ts + timedelta(seconds=page_len_s)
        map_row.end_ts = min(map_row.end_ts, max_end_ts)
        dur: timedelta = map_row.end_ts - map_row.start_ts
        map_row.duration_s = dur.total_seconds()
        page_map_rows.append(map_row)
        last_map_row = map_row

        # map_ids = []
        # has_dd2 = False

        for row in page:
            if not (last_map_row.start_ts <= row.ts <= last_map_row.end_ts):
                page_map_rows.append(map_lookup.get_map(row.ts).copy())
                last_map_row = page_map_rows[-1]
            ts.append(row.ts)
            h = row.dist3[1]
            dhs_p.append(h / row.dt if h > 0 else 0)
            dhs_n.append(h / row.dt if h < 0 else 0)
            # hs.append((row.pos0[1] + row.pos1[1]) * 0.5)
            hs.append(row.pos0[1])
            dp.append(row.dist / row.dt)
            if dhs_p[-1] > 50.0:
                print(f"... a delta height > 50: {dhs_p[-1]}")

        min_h = min(hs)
        max_h = max(hs)
        y2_range = max_h + 120

        last_map_row.end_ts = min(last_map_row.end_ts, end_ts)
        last_map_row.duration_s = (
            last_map_row.end_ts - last_map_row.start_ts
        ).total_seconds()

        page_map_ts: list = []
        page_map_names: list[str] = []
        page_map_radius: list[float] = []
        page_map_colors = [
            "red",
            "green",
            "blue",
            "purple",
            "orange",
            "cyan",
            "magenta",
        ]

        # MARK: page_map_rs

        page_duration = (
            page_map_rows[-1].end_ts - page_map_rows[0].start_ts
        ).total_seconds()
        page_y2_m_per_sec = y2_range / page_duration
        print(f"page_duration: {page_duration} s -- {page_y2_m_per_sec} m / s")
        for row in page_map_rows:
            # add 10 hrs for timestamp offset
            ets, sts = (row.end_ts.timestamp(), row.start_ts.timestamp())
            page_map_ts.append((ets + sts) / 2 + (10 * 3600))
            page_map_names.append(row.map_name or "??")
            print(f"start -> end: {row.start_ts} -> {row.end_ts}")
            radius_days = (row.end_ts - row.start_ts).total_seconds() / 86400 / 2
            page_map_radius.append(radius_days)

        colors_gen = itertools.cycle(page_map_colors)
        page_map_colors = [colors_gen.__next__() for _ in range(len(page_map_ts))]

        if max(dhs_p) > 50.0:
            print(f"max delta height > 50: {max(dhs_p)}")

        print(f"prepped data for page: {i} -- {len(ts)}")

        start: datetime = min(ts)
        end: datetime = max(ts)

        print(f"start: {start} -- end: {end}")

        dur = end - start
        if dur.total_seconds() < 2.0:
            # print(f"Session that lasted less than 2s?")
            continue

        # 2024-05-08T00:00:58.000Z
        if end < ignore_before:
            continue

        page_map_ts = mdates.epoch2num(page_map_ts)
        ts = mdates.date2num(ts)
        min_ts = min(ts)
        max_ts = max(ts)

        print(
            f"Lenghts: {len(page_map_ts)} = {len(page_map_names)} = {len(page_map_radius)}"
        )

        # points = np.array([ts, dhs]).T.reshape(-1, 1, 2)
        # print(f"P = {points}")
        # segments = np.concatenate([points[:-1], points[1:]], axis=1)
        # print(f"S = {segments}")
        t_dhs_p = list(zip(ts, dhs_p))
        t_dhs_n = list(zip(ts, dhs_n))
        t_hs = list(zip(ts, hs))
        t_dp = list(zip(ts, dp))

        # print(f"t_dhs_p: {len(t_dhs_p)}")

        # ts = np.array(ts)

        xs = np.linspace(min_ts, max_ts, int(math.ceil(dur.total_seconds() / 5)))
        # create ys for dhs_p and dhs_n in the corresponding slot only, otherwise 0
        # p = positive, n = ngtv
        y_dhs_p = np.zeros(len(xs))
        y_dhs_n = np.zeros(len(xs))
        min_j = 0
        min_k = 0

        print(f"got linspace and friends")

        for j, x in enumerate(xs):
            for k in range(max(min_k - 1, 0), len(t_dhs_p)):
                (t, dh) = t_dhs_p[k]
                # k += min_k
                if k < min_k:
                    continue
                if t < x:
                    y_dhs_p[j] = dh
                else:
                    break
            # for k, (t, dh) in enumerate(t_dhs_n[max(min_k - 1, 0) :]):
            for k in range(max(min_k - 1, 0), len(t_dhs_n)):
                (t, dh) = t_dhs_n[k]
                # k += min_k
                if k < min_k:
                    continue
                if t < x:
                    y_dhs_n[j] = dh
                else:
                    min_k = k
                    break

        print(f"got y_dhs_p and y_dhs_n")

        generate_page_indexes = True

        if generate_page_indexes:
            out_f = csv_p.parent / f"{csv_p.stem}_page_{i:03d}.csv"
            with open(out_f, "w") as f:
                f.write("ts,dhs_p,dhs_n\n")
                for j, x in enumerate(mdates.num2date(xs)):
                    x: datetime = x
                    f.write(
                        f"{x.timestamp()},{x.isoformat()},{y_dhs_p[j]},{y_dhs_n[j]}\n"
                    )

            print(f"saved: {out_f}")

            # continue

        # lc = LineCollection(
        #     [t_dhs], cmap="viridis", array=[0]
        # )  # , norm=plt.Normalize(0, 10))

        fig, ax = plt.subplots(figsize=(fig_width, 8))
        plt.xticks(rotation=45, ha="right")

        lc2 = LineCollection(
            [t_hs], cmap="viridis", colors=["blue"]
        )  # , norm=plt.Normalize(0, 10))
        y2ax = ax.twinx()

        # add the map names first as scatter plot
        zeros = [0] * len(page_map_ts)
        # print(
        #     f"page_map_ts: {len(page_map_ts)} = {len(page_map_names)} = {len(page_map_radius)} = {len(zeros)}"
        # )

        for j in range(len(page_map_ts)):
            y2ax.axvspan(
                page_map_ts[j] - page_map_radius[j],
                page_map_ts[j] + page_map_radius[j],
                alpha=0.25,
                color=page_map_colors[j],
            )
            # y2ax.scatter(
            #     [page_map_ts[j]],
            #     [0],
            #     s=page_map_radius[j],
            #     c=page_map_colors[j],
            #     alpha=0.35,
            #     marker="s",
            #     label=page_map_names[j],
            # )

        # y2ax.scatter(
        #     page_map_ts,
        #     zeros,
        #     s=page_map_radius,
        #     c=page_map_colors,
        #     alpha=0.35,
        #     marker="s",
        #     label=
        # )

        lab_row = 0
        label_height_offs = [100, 50, 33, 66, 0, 42, 84, 58, 16, 75, 25]

        for j in range(len(page_map_ts)):
            y2ax.annotate(
                page_map_names[j],
                (page_map_ts[j], max_h + label_height_offs[lab_row]),
                ha="center",
                va="center",
                fontsize=10,
                color="black",
                arrowprops=dict(facecolor="black", shrink=0.05),
            )
            # y2ax.text(
            #     page_map_ts[j],
            #     max_h + (100 if top_row else 50),
            #     page_map_names[j],
            #     ha="center",
            #     va="center",
            #     fontsize=10,
            #     color="black",
            # )
            lab_row = (lab_row + 1) % 11

        y2ax.add_collection(lc2)
        y2ax.autoscale()
        # y2ax.set_ybound(0.0, max_h + 120)
        y2ax.set_ylim(0.0, max_h + 140)
        # y2ax.set_xbound(None, None)
        y2ax.set_ylabel("Height (m)")

        ax.bar(xs, y_dhs_p, width=0.0001, color="green", alpha=0.5)
        ax.bar(xs, y_dhs_n, width=0.0001, color="red", alpha=0.5)
        # ax.set_ylim(min(dhs_n), max(dhs_p))

        ax.autoscale()
        ax.set_xbound(None, None)
        # ax.set_ybound(-200, 500)
        ax.set_ylabel("Delta Height (m/s) between updates")
        ax.xaxis.set_major_locator(mdates.MinuteLocator(interval=5))
        ax.xaxis.set_major_formatter(mdates.DateFormatter("%Y-%m-%d %H:%M:%S %Z"))
        title_text = plt.title(
            f"Pos Deltas: {user_name} ({dur.total_seconds() / 60.:.1f} min) -- {start.isoformat()} to {end.isoformat()}",
            fontdict={"fontsize": 10},
        )

        plt.tight_layout()

        out_f = csv_p.parent / f"{csv_p.stem}_page_{i:03d}.png"
        plt.savefig(out_f)
        plt.close()
        print(f"saved: {out_f}")
        # sys.exit(0)

        # if i > 3: break
        # break
    # break
