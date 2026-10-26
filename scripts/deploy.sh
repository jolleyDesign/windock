#!/usr/bin/env bash
# Build Windock from WSL, install it to %LOCALAPPDATA%\Programs\Windock and (re)start it.
#   scripts/deploy.sh          build release + install + start
#   scripts/deploy.sh --stop   just stop the running dock
set -euo pipefail
cd "$(dirname "$0")/.."

win_env() { cmd.exe /c "echo %$1%" 2>/dev/null | tr -d '\r'; }

# Not `tasklist | grep -q`: with pipefail, grep exiting early makes the pipeline fail.
running() { [[ "$(tasklist.exe /FI "IMAGENAME eq windock.exe" 2>/dev/null)" == *windock.exe* ]]; }

stop() {
    if running; then
        taskkill.exe /IM windock.exe /F >/dev/null
    fi
    # The exe stays locked until every instance (dock and settings window) has exited.
    for _ in $(seq 50); do
        running || return 0
        sleep 0.1
    done
}

if [[ "${1:-}" == "--stop" ]]; then
    stop
    exit 0
fi

cargo xwin build --release

dest_win="$(win_env LOCALAPPDATA)\\Programs\\Windock"
dest="$(wslpath -u "$dest_win")"
mkdir -p "$dest"
stop
cp target/x86_64-pc-windows-msvc/release/windock.exe "$dest/"
powershell.exe -NoProfile -Command "Start-Process '$dest_win\\windock.exe'"
echo "Windock running from $dest_win\\windock.exe"
echo "Config: $(win_env APPDATA)\\Windock\\config.toml"
