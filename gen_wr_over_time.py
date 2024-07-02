from collections import defaultdict
import json
from pathlib import Path
import csv
from dataclasses import dataclass
from datetime import datetime, timedelta, tzinfo
import matplotlib
from matplotlib.cbook import flatten
import matplotlib.dates
import matplotlib.pyplot as plt
from matplotlib.transforms import Affine2D, Transform
import matplotlib.transforms
import numpy as np
from matplotlib.collections import LineCollection
import matplotlib.dates as mdates

import itertools

print(f"\n\n\n---------      gen_wr_over_time.py        ---------\n\n\n")

# wr_time_csv = Path("output/wr-over-time.csv")
wr_time_csv = Path("output/wr_over_time_vs.csv")
# ts,height,wsid,name,color


@dataclass
class Row:
    ts: float
    height: float
    wsid: str
    name: str
    color: str

    @classmethod
    def from_csv_row(cls, row: list[str]):
        _ts: datetime = datetime.fromtimestamp(float(row[0]), tz=matplotlib.dates.UTC)
        return cls(
            ts=matplotlib.dates.date2num(_ts),
            height=float(row[1]),
            wsid=row[2],
            name=row[3],
            color=row[4],
        )

    def copy(self) -> "Row":
        return Row(
            ts=self.ts,
            height=self.height,
            wsid=self.wsid,
            name=self.name,
            color=self.color,
        )


@dataclass
class GroupInfo:
    wsid: str
    color: str
    name: str

    @classmethod
    def from_csv_row(cls, *row: list[str]):
        return cls(
            wsid=row[0],
            color=row[1],
            name=row[2],
        )


def get_human_time(end: float, start: float = 0) -> str:
    end = matplotlib.dates.num2date(end)
    start = matplotlib.dates.num2date(start)
    delta = end - start
    return str(delta)


p: Path = wr_time_csv
reader = csv.reader(p.read_text().splitlines())
headings = reader.__next__()
rows: list[Row] = [Row.from_csv_row(row) for row in reader]

# def process_groups(rows: iter[tuple[str, list[Row]]]):
#     for wsid, group in rows:
#         print(f"wsid: {wsid}")
#         for row in group:
#             print(f"row: {row}")

series: list[tuple[GroupInfo, list[float], list[float], list[Row]]] = []

for wsid, group in itertools.groupby(rows, lambda r: r.wsid):
    g = list(group)
    fst = g[0]
    gi = GroupInfo.from_csv_row(wsid, fst.color, fst.name)
    ts = [r.ts for r in g]
    ys = [r.height for r in g]
    if len(series) == 0:
        series.append([gi, ts, ys, g])
    else:
        lst = series[-1][3][-1]
        if lst.ts > ts[0]:
            series.append([gi, ts, ys, g])
        else:
            new_head = lst.copy()
            new_head.name = fst.name
            new_head.color = fst.color
            new_head.wsid = wsid
            series.append(
                [gi, [new_head.ts] + ts, [new_head.height] + ys, [new_head] + g]
            )
    _, ts, ys, _ = series[-1]
    # make right angles for all corners
    series[-1][1] = [ts[0]] + list(flatten(itertools.pairwise(ts)))
    series[-1][2] = list(flatten(itertools.pairwise(ys))) + [ys[-1]]
    # series[-1][2].pop()
    if ys[-1] > 1904:
        series[-1][2][-1] = 1910
        break


# while series[0][2][-1] < 35:
#     series.pop(0)

# series[0][0].name = "***0***"
# series[1][0].name = "***1***"
# series[2][0].name = "***2***"
# series[3][0].name = "***3***"

user_colors = dict()
seen_colors: set = {matplotlib.colors.TABLEAU_COLORS.__iter__().__next__()}


def get_user_color(name: str, default_c: str) -> str:
    if name in user_colors:
        return user_colors[name]
    if default_c not in seen_colors:
        seen_colors.add(default_c)
        user_colors[name] = default_c
        return default_c
    for c in matplotlib.colors.TABLEAU_COLORS:
        if c not in seen_colors:
            seen_colors.add(c)
            user_colors[name] = c
            return c
    return "#000000"


name_seen_log = []


def name_to_ix(name: str) -> int:
    try:
        return name_seen_log.index(name)
    except ValueError:
        name_seen_log.append(name)
        return len(name_seen_log) - 1


markers = [
    "*",
    "o",
    "s",
    "D",
    "v",
    "^",
    "H",
    "<",
    ">",
    "p",
    "P",
    "*",
    "X",
    "d",
    "H",
    "h",
    "8",
]


players_total_time_holding_wr = defaultdict(lambda: 0.0)
players_total_height_discovered = defaultdict(lambda: 0.0)
players_total_wr_held_count = defaultdict(lambda: 0)

for gi, ts, ys, g in series:
    players_total_time_holding_wr[gi.name] += ts[-1] - ts[0]
    players_total_height_discovered[gi.name] += ys[-1] - ys[0]
    players_total_wr_held_count[gi.name] += 1


name_count = defaultdict(lambda: 0)


def name_with_occur_count(name: str, ts: list[float], ys) -> str:
    name_count[name] += 1
    duration_str = get_human_time(ts[-1], ts[0])
    return f"{name} (#{players_total_wr_held_count[name] - name_count[name] + 1} / {duration_str} / {ys[-1]:.1f} m / +{ys[-1] - ys[0]:.2f} m)"


fig, ax = plt.subplots(figsize=(20, 10.5))
ax.set_xbound(None, None)
ax.set_ylabel("Height (m)")

# draw background heights:
floor_heights = [
    8.0,
    104.0,
    208.0,
    312.0,
    416.0,
    520.0,
    624.0,
    728.0,
    832.0,
    936.0,
    1040.0,
    1144.0,
    1264.0,
    1376.0,
    1480.0,
    1584.0,
    1688.0,
    1793.0,
    1910.0,
]
# prep yticks
ax.set_yticks(floor_heights)
y_labs = []
for i, h in enumerate(floor_heights):
    ax.axhline(h, color="#00000044", linewidth=0.5, linestyle="-")
    # floor label on RHS
    lab_name = (
        "Floor Gang"
        if i == 0
        else (
            f"F{i:02}"
            if i < 17
            else "End" if i == 17 else "Finish" if i == 18 else f"??? {i}"
        )
    )
    # ax.text(
    #     ts[-1] + 2,
    #     h - 10,
    #     f"{int(h):04} m - {lab_name}",
    #     fontsize=10,
    #     color="black",
    #     ha="left",
    # )
    y_labs.append(f"{lab_name} - {int(h)}")
ax.set_yticklabels(y_labs)

min_ts = series[0][1][0]
max_ts = series[-1][1][-1]

for i, (gi, ts, ys, g) in enumerate(series[::-1]):
    # print(f"{type(ts)}")
    # print(f"{type(ys)}")
    ax.plot(
        ts,
        ys,
        f"-{markers[name_to_ix(gi.name)]}",
        label=name_with_occur_count(gi.name, ts, ys),
        color=get_user_color(gi.name, gi.color),
        markersize=3,
        linewidth=1.5,
        # zorder=len(series) - i,
    )
    ax.axvspan(ts[0], ts[-1], alpha=0.15, color=get_user_color(gi.name, gi.color))

ax.autoscale()
ax.xaxis.set_major_locator(mdates.DayLocator(interval=1))
ax.xaxis.set_major_formatter(mdates.DateFormatter("%Y-%m-%d %H:%M:%S %Z"))
# ax.legend([])
ax.set_ylim(0, 1950)

leg = ax.legend(fontsize=10)
plt.xticks(rotation=45, ha="right")

title_text = plt.title(
    f"Deep Dip 2: Approx WR Height by Date  [{get_human_time(max_ts, min_ts)}]\n(Note: Early LBs were not accurate and thus are partially redacted)",
    fontdict={"fontsize": 14},
)

plt.tight_layout()

# draw stats in little box

stats_list = [
    f"{name} -- # Times Held: {players_total_wr_held_count[name]} -- Total Time: {get_human_time(players_total_time_holding_wr[name])} -- Discovered: {players_total_height_discovered[name]:.2f} m"
    for name in name_seen_log
]

stats_list = [" " * int(len(stats_list[0]) * 1.5), "", "", "", "", "", ""]
extra_stats = "\n".join(stats_list)

# half width of table
table_hw = 0.174 * 1.4
table_h = 0.39
table_w = table_hw * 2


def draw_stats_box():
    # these are matplotlib.patch.Patch properties
    props = dict(
        boxstyle="round",
        facecolor="#0072bc",
        alpha=0.3,
        # width=table_w + 0.03,
        # widht=1,
        edgecolor="black",
    )
    # place a text box in upper left in axes coords
    _t = ax.text(
        0.5,
        table_h + 0.03,
        extra_stats,
        transform=ax.transAxes,
        fontsize=12,
        verticalalignment="top",
        horizontalalignment="center",
        bbox=props,
        linespacing=2.0,
    )
    _t.get_bbox_patch().set_width(table_w + 0.03)


# prep table
cell_text = []
# ["Name", "# WR Claimed", "Total Time Held", "Height Discovered"]
print(f"{name_seen_log}")
for i, name in enumerate(name_seen_log):
    cell_text.append(
        [
            name,
            players_total_wr_held_count[name],
            get_human_time(players_total_time_holding_wr[name]),
            f"{players_total_height_discovered[name]:.2f} m",
        ]
    )
cell_labels = ["", "Times WR Claimed", "Total Time Held", "Height Discovered*"]
stats_table = plt.table(
    cellText=cell_text,
    colLabels=cell_labels,
    cellLoc="center",
    loc="lower center",
    colLoc="center",
    rowLoc="center",
    colWidths=[table_hw / 2] * 4,
    bbox=[0.5 - table_hw, 0.01, table_hw * 2, table_h],
    alpha=1.0,
    zorder=10,
    cellColours=([["#222A"] * 4, ["#3338"] * 4] * 8)[:15],
    colColours=["#3338"] * 4,
    # transform=matplotlib.transforms.Affine2D(),  # $.translate(0, 0),
)


def set_pad_for_column(col, pad=0.1):
    cells = stats_table.get_celld()
    column = [cell for cell in stats_table.get_celld() if cell[1] == col]
    for cell in column:
        cells[cell].PAD = pad
        bs = cells[cell].get_bbox().bounds
        _t = cells[cell].get_text()
        cells[cell].set_text_props(
            color="white",
            fontsize=12,
            fontweight="bold",
            verticalalignment="center",
        )


set_pad_for_column(col=0, pad=0.1)
set_pad_for_column(col=1, pad=0.1)
set_pad_for_column(col=2, pad=0.1)
set_pad_for_column(col=3, pad=0.1)


# draw_stats_box()

out_f = p.parent / f"{p.stem}.png"
plt.savefig(out_f)
plt.close()
