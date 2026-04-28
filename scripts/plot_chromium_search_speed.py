#!/usr/bin/env python3

import argparse
import csv
from collections import defaultdict
from pathlib import Path
import textwrap

import matplotlib.pyplot as plt
import numpy as np


FLASHGREP_COLOR = "#2F7D6B"
RG_COLOR = "#C46A3F"
GRID_COLOR = "#E5E9ED"
TEXT_COLOR = "#333940"
TICK_COLOR = "#4E5968"


def load_first(path: Path) -> dict[str, dict[str, float]]:
    values: dict[str, dict[str, float]] = defaultdict(dict)
    with path.open() as fh:
        for row in csv.DictReader(fh):
            values[row["benchmark"]][row["runner"]] = float(row["duration_secs"])
    return dict(values)


def load_warm_mean(path: Path) -> dict[str, dict[str, float]]:
    grouped: dict[str, dict[str, list[float]]] = defaultdict(lambda: defaultdict(list))
    with path.open() as fh:
        for row in csv.DictReader(fh):
            grouped[row["benchmark"]][row["runner"]].append(float(row["duration_secs"]))

    values: dict[str, dict[str, float]] = {}
    for benchmark, runners in grouped.items():
        values[benchmark] = {
            runner: sum(samples) / len(samples) for runner, samples in runners.items()
        }
    return values


def friendly_kind_label(kind: str, pattern: str) -> str:
    label_map = {
        "cpp_literal": "Literal symbol",
        "cpp_word": "Whole-word regex",
        "feature_literal": "Feature symbol",
        "browser_test_macro": "Browser test macro",
        "alternates_core_types": "Core types alternation",
        "export_macro_regex": "Export macro regex",
        "browser_test_name_regex": "BrowserTest regex",
        "header_string": "Header string",
    }
    if kind in label_map:
        return label_map[kind]
    return pattern


def load_pattern_labels(path: Path) -> dict[str, str]:
    labels: dict[str, str] = {}
    with path.open() as fh:
        reader = csv.reader(fh, delimiter="\t")
        for benchmark, _kind, pattern in reader:
            labels[benchmark] = friendly_kind_label(_kind, pattern)
    return labels


def prettify_pattern(pattern: str) -> str:
    display = pattern.replace("\\\\", "\\")
    if len(display) > 24:
        return "\n".join(
            textwrap.wrap(display, width=24, break_long_words=False, break_on_hyphens=False)
        )
    return display


def format_secs(value: float) -> str:
    if value >= 10:
        return f"{value:.1f}s"
    if value >= 1:
        return f"{value:.2f}s"
    if value >= 0.1:
        return f"{value:.3f}s"
    if value >= 0.01:
        return f"{value:.3f}s"
    return f"{value:.4f}s"


def log_ticks(min_value: float, max_value: float) -> list[float]:
    candidates = [0.001, 0.01, 0.1, 1.0, 10.0, 100.0]
    lower = min_value / 1.7
    upper = max_value * 2.0
    ticks = [tick for tick in candidates if lower <= tick <= upper]
    if not ticks:
        return [min_value, max_value]
    return ticks


def draw_panel(
    ax,
    title: str,
    values: dict[str, dict[str, float]],
    benchmarks: list[str],
    pattern_labels: dict[str, str],
) -> None:
    labels = [prettify_pattern(pattern_labels.get(benchmark, benchmark)) for benchmark in benchmarks]
    flashgrep = [values[b]["flashgrep"] for b in benchmarks]
    rg = [values[b]["rg"] for b in benchmarks]

    y = np.arange(len(benchmarks))
    height = 0.34

    flashgrep_bars = ax.barh(
        y - height / 2,
        flashgrep,
        height,
        color=FLASHGREP_COLOR,
        label="flashgrep",
    )
    rg_bars = ax.barh(y + height / 2, rg, height, color=RG_COLOR, label="ripgrep")

    ax.set_yticks(y, labels, fontsize=12, color=TEXT_COLOR)
    ax.set_xscale("log")
    ax.grid(axis="x", which="major", color=GRID_COLOR, linewidth=1)
    ax.set_axisbelow(True)
    ax.tick_params(axis="x", labelsize=12, colors=TICK_COLOR)
    ax.tick_params(axis="y", colors=TICK_COLOR, length=0, pad=10, labelsize=13)
    ax.spines["top"].set_visible(False)
    ax.spines["right"].set_visible(False)
    ax.spines["left"].set_color(TICK_COLOR)
    ax.spines["bottom"].set_color(TICK_COLOR)
    ax.spines["left"].set_linewidth(1.6)
    ax.spines["bottom"].set_linewidth(1.6)
    ax.invert_yaxis()

    min_value = min(min(flashgrep), min(rg))
    max_value = max(max(flashgrep), max(rg))
    ax.set_xlim(min_value / 1.7, max_value * 2.0)
    ticks = log_ticks(min_value, max_value)
    ax.set_xticks(ticks, labels=[format_secs(tick) for tick in ticks])

    for bars in (flashgrep_bars, rg_bars):
        for bar in bars:
            value = bar.get_width()
            ax.text(
                value * 1.05,
                bar.get_y() + bar.get_height() / 2,
                format_secs(value),
                ha="left",
                va="center",
                fontsize=11,
                fontweight="bold",
                color=TEXT_COLOR,
            )


def plot_single(
    output: Path,
    panel_title: str,
    subtitle: str,
    values: dict[str, dict[str, float]],
    pattern_labels: dict[str, str],
) -> None:
    benchmarks = sorted(values.keys(), key=lambda item: int(item.rsplit("_", 1)[1]))

    plt.rcParams.update(
        {
            "figure.facecolor": "white",
            "axes.facecolor": "white",
            "font.family": "sans-serif",
            "font.sans-serif": ["Helvetica", "Arial", "Hiragino Sans GB", "DejaVu Sans"],
        }
    )

    fig, ax = plt.subplots(figsize=(18, 10.6), dpi=100)
    fig.subplots_adjust(left=0.19, right=0.97, top=0.84, bottom=0.10)
    fig.text(0.055, 0.965, "flashgrep and ripgrep on chromium-src", fontsize=33, color=TEXT_COLOR)
    fig.text(0.055, 0.915, subtitle, fontsize=16, color=TICK_COLOR)

    draw_panel(ax, panel_title, values, benchmarks, pattern_labels)

    handles, labels = ax.get_legend_handles_labels()
    fig.legend(
        handles,
        labels,
        loc="upper right",
        ncol=2,
        frameon=False,
        fontsize=17,
        handlelength=0.8,
        handletextpad=0.5,
        columnspacing=1.0,
        bbox_to_anchor=(0.93, 0.985),
    )

    output.parent.mkdir(parents=True, exist_ok=True)
    fig.savefig(output, bbox_inches="tight")
    plt.close(fig)


def build_charts(
    first_csv: Path,
    warm_csv: Path,
    pattern_map: Path,
    first_output: Path,
    warm_output: Path,
) -> None:
    first_values = load_first(first_csv)
    warm_values = load_warm_mean(warm_csv)
    pattern_labels = load_pattern_labels(pattern_map)
    plot_single(
        first_output,
        "First Search",
        "workspace-eventual top-level service path, first query",
        first_values,
        pattern_labels,
    )
    plot_single(
        warm_output,
        "Warm Search",
        "workspace-eventual top-level service path, warm query mean",
        warm_values,
        pattern_labels,
    )


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Plot chromium flashgrep vs ripgrep benchmark charts from benchmark CSV files."
    )
    parser.add_argument("--first-csv", required=True, type=Path)
    parser.add_argument("--warm-csv", required=True, type=Path)
    parser.add_argument("--pattern-map", required=True, type=Path)
    parser.add_argument("--first-output", required=True, type=Path)
    parser.add_argument("--warm-output", required=True, type=Path)
    return parser.parse_args()


def main() -> None:
    args = parse_args()
    build_charts(
        args.first_csv,
        args.warm_csv,
        args.pattern_map,
        args.first_output,
        args.warm_output,
    )


if __name__ == "__main__":
    main()
