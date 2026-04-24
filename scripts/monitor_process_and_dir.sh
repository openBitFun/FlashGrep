#!/usr/bin/env bash
set -euo pipefail

if [[ $# -lt 3 || $# -gt 4 ]]; then
  echo "usage: monitor_process_and_dir.sh <pid> <dir> <output_csv> [interval_secs]" >&2
  exit 2
fi

pid="$1"
dir="$2"
output_csv="$3"
interval="${4:-2}"

mkdir -p "$(dirname "$output_csv")"

if [[ ! -f "$output_csv" ]]; then
  printf 'epoch,pid_alive,pcpu,pmem,etime,state,dir_kb\n' > "$output_csv"
fi

while true; do
  epoch="$(date +%s)"
  if ps_out="$(ps -p "$pid" -o pid=,pcpu=,pmem=,etime=,state= 2>/dev/null)"; then
    read -r _pid pcpu pmem etime state <<< "$ps_out"
    pid_alive=1
  else
    pcpu=""
    pmem=""
    etime=""
    state=""
    pid_alive=0
  fi

  if dir_out="$(du -sk "$dir" 2>/dev/null)"; then
    dir_kb="$(awk '{print $1}' <<< "$dir_out")"
  else
    dir_kb=0
  fi

  printf '%s,%s,%s,%s,%s,%s,%s\n' \
    "$epoch" "$pid_alive" "$pcpu" "$pmem" "$etime" "$state" "$dir_kb" >> "$output_csv"

  if [[ "$pid_alive" == "0" ]]; then
    break
  fi
  sleep "$interval"
done
