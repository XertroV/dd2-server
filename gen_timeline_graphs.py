from pathlib import Path
import csv
from dataclasses import dataclass
from datetime import datetime, timedelta
import matplotlib.pyplot as plt
import numpy as np
from matplotlib.collections import LineCollection
import matplotlib.dates as mdates


output_tl = Path("output/timeline")
otl_users = list(output_tl.glob("*"))

print(f"otl_users_len {len(otl_users)}")

user_csvs: list[(Path, list[Path])] = []

for user_tl_folder in otl_users:
    # print(f"Processing: {user_tl_folder}")
    csvs = list(user_tl_folder.glob("*.csv"))
    # print(f"nb csvs: {len(csvs)}")
    user_csvs.append((user_tl_folder, csvs))


@dataclass
class TimelineRow:
    ROWS = ["start_ts", "end_ts", "ty", "map_id", "is_dd2_uid_or_item"]
    start: datetime
    end: datetime | None
    ty: int
    map_id: int
    is_dd2: bool

    @classmethod
    def from_row(cls, row: list[str]) -> "TimelineRow":
        row = [e.strip() for e in row]
        # print(f"from_row: {row}")
        return cls(
            datetime.fromtimestamp(int(row[0])),
            datetime.fromtimestamp(int(row[1])) if len(row[1]) > 0 else None,
            int(row[2]),
            int(row[3]),
            int(row[4]) > 0,
        )

    def get_col(self) -> str:
        return ""


import itertools

ignore_before = datetime.fromtimestamp(1715126458)

user_csvs = sorted(user_csvs, key=lambda p: p[0].stem)
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
        rows = [TimelineRow.from_row(line) for line in reader]
        if len(rows) == 0:
            print(f"Warning: no rows: {p}")
            continue
        ys = []
        ts = []
        cs = []
        map_ids = []
        has_dd2 = False
        for row in rows:
            ts.append(row.start)
            ts.append(
                row.end if row.end is not None else (row.start + timedelta(seconds=1))
            )
            for _ in [0, 1]:
                map_ids.append(row.map_id)
                if row.is_dd2:
                    has_dd2 = True
                # offset type if DD2
                ys.append(row.ty + (6 if row.is_dd2 and row.ty > 8 else 0))
                if row.is_dd2 and row.ty > 8 and row.start > ignore_before:
                    print(f"DD2 type: {row.ty}")
                cs.append(row.get_col())

        uniq_ids = set(map_ids)

        start: datetime = min(ts)
        end: datetime = max(ts)

        dur = end - start
        if dur.total_seconds() < 2.0:
            # print(f"Session that lasted less than 2s?")
            continue

        # 2024-05-08T00:00:58.000Z
        if start < ignore_before:
            continue

        # 7 = unk, 9 = start of editor range
        has_any_editor = any([y > 8 for y in ys])
        if not has_any_editor:
            continue

        ts = mdates.date2num(ts)

        # print(f"ys: {ys}")
        # print(f"ts: {ts}")

        points = np.array([ts, ys]).T.reshape(-1, 1, 2)
        segments = np.concatenate([points[:-1], points[1:]], axis=1)

        segs = [list(zip(ts, ys))]
        # print(f"segs: {segs}")
        # print(f"segs: {segments}")

        ts = np.array(ts)

        lc = LineCollection(
            segments, cmap="viridis", norm=plt.Normalize(0, 10)
        )  # , norm=plt.Normalize(0, 10))

        fig, ax = plt.subplots(figsize=(20, 8))
        ax.set_xbound(None, None)
        ax.set_ylim(-1, 19)
        ax.set_ylabel("Context type")
        # we offset editor DD2 types earlier
        ax.set_yticks(
            [0, 2, 3, 5, 7, 9, 10, 11, 12, 15, 16, 17, 18],
            [
                "MainMenu",
                "MapDriving",
                "MapMenu",
                "MapUnk",
                "Unk",
                "EditorUnk",
                "EditorMap",
                "EditorMT",
                "EditorDriving",
                "EdUnkDD2",
                "EdMapDD2",
                "EdMtDD2",
                "EdDriveDD2",
            ],
        )
        ax.add_collection(lc)
        ax.autoscale()
        ax.xaxis.set_major_locator(mdates.MinuteLocator(interval=5))
        ax.xaxis.set_major_formatter(mdates.DateFormatter("%Y-%m-%d %H:%M:%S %Z"))
        plt.xticks(rotation=45, ha="right")
        map_ids = list(uniq_ids)
        _map_ids = ("len=" + str(len(map_ids))) if len(map_ids) > 10 else str(map_ids)
        title_text = plt.title(
            f"Timeline: {user_name} ({dur.total_seconds() / 60.:.1f} min) -- {start.isoformat()} to {end.isoformat()} (MapIds = {_map_ids} / Has a DD2 map = {has_dd2})",
            fontdict={"fontsize": 10},
        )
        plt.tight_layout()

        out_f = p.parent / f"{p.stem}.png"
        plt.savefig(out_f)
        plt.close()

        # if i > 3: break
        # break
    # break
