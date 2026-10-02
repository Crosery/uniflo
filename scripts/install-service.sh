#!/usr/bin/env bash
# Install uniflo daemon as a macOS launchd user service (auto-start on login)
set -euo pipefail

PLIST_NAME="com.crosery.uniflo.plist"
TARGET_DIR="${HOME}/Library/LaunchAgents"
TARGET_PLIST="${TARGET_DIR}/${PLIST_NAME}"
BIN_PATH="$(command -v uniflo || echo "${HOME}/.cargo/bin/uniflo")"
LOG_PATH="${HOME}/Library/Logs/uniflo.log"

if [[ ! -x "${BIN_PATH}" ]]; then
  echo "Error: uniflo binary not found at ${BIN_PATH}. Run cargo install --path crates/uniflo-cli first." >&2
  exit 1
fi

mkdir -p "${TARGET_DIR}" "${HOME}/Library/Logs"

if launchctl list | grep -q "com.crosery.uniflo"; then
  echo "Unloading existing service..."
  launchctl unload "${TARGET_PLIST}" 2>/dev/null || true
fi

cat > "${TARGET_PLIST}" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.crosery.uniflo</string>
    <key>ProgramArguments</key>
    <array>
        <string>${BIN_PATH}</string>
        <string>daemon</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>StandardOutPath</key>
    <string>${LOG_PATH}</string>
    <key>StandardErrorPath</key>
    <string>${LOG_PATH}</string>
    <key>EnvironmentVariables</key>
    <dict>
        <key>PATH</key>
        <string>/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin:${HOME}/.cargo/bin</string>
    </dict>
</dict>
</plist>
EOF

launchctl load "${TARGET_PLIST}"
echo "uniflo launchd service installed and started."
echo "Log: ${LOG_PATH}"
