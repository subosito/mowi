#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
command -v tui-test >/dev/null || { echo 'tui-test is required: https://github.com/microsoft/tui-test' >&2; exit 77; }
cargo build -q

run_smoke() {
    local theme=$1
    local session="mowi-smoke-${theme//[^a-z0-9]/-}-$$"
    tui-test run --session "$session" --cols 100 --rows 30 \
        --env NO_COLOR=1 --env MOW_NO_ANIM=1 --env MOW_BIN="$PWD/tests/fixtures/mock-mow" \
        "$PWD/target/debug/mowi" --theme "$theme" --continue >/dev/null
    tui-test wait text --session "$session" --regex --timeout 10000 'resumed answer' >/dev/null
    tui-test press --session "$session" Escape
    tui-test type --session "$session" 'draft-after-scroll'
    tui-test expect text --session "$session" 'draft-after-scroll' >/dev/null
    tui-test press --session "$session" PageUp PageDown
    tui-test expect text --session "$session" 'draft-after-scroll' >/dev/null
    tui-test resize --session "$session" 45 12
    tui-test expect text --session "$session" 'fixture-model' >/dev/null
    tui-test press --session "$session" Ctrl+C
    tui-test wait exit --session "$session" --timeout 10000 >/dev/null
}

run_progress_smoke() {
    local session="mowi-smoke-progress-$$"
    tui-test run --session "$session" --cols 100 --rows 30 \
        --env NO_COLOR=1 --env MOW_NO_ANIM=1 --env MOW_BIN="$PWD/tests/fixtures/mock-mow" \
        "$PWD/target/debug/mowi" --theme catppuccin-mocha >/dev/null
    tui-test wait text --session "$session" --regex --timeout 10000 'fixture-model' >/dev/null
    tui-test press --session "$session" Escape
    tui-test type --session "$session" 'please edit the guard'
    tui-test press --session "$session" Enter
    # Live tokens, then the write/edit diff card, then the folded answer.
    tui-test wait text --session "$session" --regex --timeout 10000 'Looking at the guard' >/dev/null
    tui-test wait text --session "$session" --regex --timeout 10000 'src/app.rs' >/dev/null
    tui-test wait text --session "$session" --regex --timeout 10000 'Updated the exclusive slice' >/dev/null
    tui-test press --session "$session" Ctrl+C
    tui-test wait exit --session "$session" --timeout 10000 >/dev/null
}

for theme in catppuccin-mocha catppuccin-latte gruvbox-dark monokai; do
    run_smoke "$theme"
done
run_progress_smoke
