#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
command -v shell-use >/dev/null || { echo 'shell-use is required: https://github.com/microsoft/shell-use' >&2; exit 77; }
cargo build -q

run_smoke() {
    local theme=$1
    local session="mowi-smoke-${theme//[^a-z0-9]/-}-$$"
    shell-use run --session "$session" --cols 100 --rows 30 \
        --env NO_COLOR=1 --env MOW_NO_ANIM=1 --env MOW_BIN="$PWD/tests/fixtures/mock-mow" \
        "$PWD/target/debug/mowi" --theme "$theme" --continue >/dev/null
    shell-use wait text --session "$session" --regex 'resumed answer' --timeout 10000 >/dev/null
    shell-use press --session "$session" Escape
    shell-use type --session "$session" 'draft-after-scroll'
    shell-use expect text --session "$session" 'draft-after-scroll' >/dev/null
    shell-use press --session "$session" PageUp PageDown
    shell-use expect text --session "$session" 'draft-after-scroll' >/dev/null
    shell-use resize --session "$session" 45 12
    shell-use expect text --session "$session" 'fixture-model' >/dev/null
    shell-use press --session "$session" Ctrl+C
    shell-use wait exit --session "$session" --timeout 10000 >/dev/null
}

run_progress_smoke() {
    local session="mowi-smoke-progress-$$"
    shell-use run --session "$session" --cols 100 --rows 30 \
        --env NO_COLOR=1 --env MOW_NO_ANIM=1 --env MOW_BIN="$PWD/tests/fixtures/mock-mow" \
        "$PWD/target/debug/mowi" --theme catppuccin-mocha >/dev/null
    shell-use wait text --session "$session" --regex 'fixture-model' --timeout 10000 >/dev/null
    shell-use press --session "$session" Escape
    shell-use type --session "$session" 'please edit the guard'
    shell-use press --session "$session" Enter
    # Live tokens, then the write/edit diff card, then the folded answer.
    shell-use wait text --session "$session" --regex 'Looking at the guard' --timeout 10000 >/dev/null
    shell-use wait text --session "$session" --regex 'src/app.rs' --timeout 10000 >/dev/null
    shell-use wait text --session "$session" --regex 'Updated the exclusive slice' --timeout 10000 >/dev/null
    shell-use press --session "$session" Ctrl+C
    shell-use wait exit --session "$session" --timeout 10000 >/dev/null
}

for theme in catppuccin-mocha catppuccin-latte gruvbox-dark monokai; do
    run_smoke "$theme"
done
run_progress_smoke
