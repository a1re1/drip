#!/bin/sh
# drip status-line example — "<model> · <cwd basename>".
#
# drip feeds one JSON document on stdin and uses this script's stdout as the
# TUI status row (see README.md, "Custom status line"). POSIX sh + sed only.
set -eu

input=$(cat)

model=$(printf '%s' "$input" | sed -n 's/.*"display_name":"\([^"]*\)".*/\1/p')
dir=$(printf '%s' "$input" | sed -n 's/.*"current_dir":"\([^"]*\)".*/\1/p')

[ -n "$dir" ] && dir=$(basename "$dir")

if [ -n "$model" ] && [ -n "$dir" ]; then
    printf '\033[36m%s\033[0m \033[2m· %s\033[0m' "$model" "$dir"
elif [ -n "$model" ]; then
    printf '\033[36m%s\033[0m' "$model"
else
    printf 'drip'
fi
