#!/usr/bin/env bash
# Connects (or disconnects) this machine's Claude Code to a MemoryVault server.
#
#   connect.sh connect --url URL [--token TOKEN] [--device ID] [--injection lean|full]
#       builds mem-hook, writes ~/.memory/config, installs the hook plugin, registers the MCP server and
#       pre-approves its tools. Without --token the token already in ~/.memory/config is kept. --injection
#       writes MEM_INJECTION: "full" is the budgeted context block at session start, "lean" only the profile,
#       open work and a short rubric for the search tools; without it the setting in the config (or, if there
#       is none, the per-user default on the server) stays.
#   connect.sh disconnect [--purge]
#       undoes all of that and blanks the URL and token in ~/.memory/config; --purge deletes ~/.memory too
#       (including undelivered events in ~/.memory/spool).
#   connect.sh probe [--url URL] [--token TOKEN]
#       exit 0 when the token authenticates against the server (defaults come from ~/.memory/config)
#   connect.sh status
#       what is configured and whether it works, plus the tokens served by recent context and recall calls
#
# Everything is idempotent. Settings touched: ~/.claude.json (MCP server "memory", user scope),
# ~/.claude/settings.json (plugin, marketplace and the allow rule for the plugin's MCP tools, through the
# claude CLI and a JSON edit), the plugin's own settings (URL in settings.json, token in the keychain or
# ~/.claude/.credentials.json) and ~/.memory/config. Set MEMORY_HOME to use another memory home.
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
repo=$(cd "$here/../.." && pwd)
memory_home=${MEMORY_HOME:-$HOME/.memory}
config=$memory_home/config
claude_settings=${CLAUDE_CONFIG_DIR:-$HOME/.claude}/settings.json
marketplace=memoryvault
plugin=memory
# The plugin's MCP server is mcp__plugin_<plugin>_<server>. The legacy rule belongs to the user-scope entry
# that earlier versions of this script (and the README's manual steps) registered.
allow_rule='mcp__plugin_memory_memory__*'
legacy_allow_rule='mcp__memory__*'

die() { echo "error: $*" >&2; exit 1; }
note() { echo "  $*"; }
have() { command -v "$1" >/dev/null 2>&1; }

# Value of KEY in the config file (the last uncommented assignment), or nothing.
cfg_get() { [ -f "$config" ] && sed -n "s/^$1=//p" "$config" | tail -n 1 || true; }

# Sets KEY=VALUE in the config file: replaces an existing assignment, else appends one. Mode stays 0600.
cfg_set() {
  local key=$1 value=$2 tmp
  mkdir -p "$memory_home"
  chmod 700 "$memory_home"
  touch "$config"
  chmod 600 "$config"
  tmp=$(mktemp "$memory_home/config.XXXXXX")
  if grep -q "^$key=" "$config"; then
    V=$value awk -v k="$key" 'index($0, k "=") == 1 { print k "=" ENVIRON["V"]; next } { print }' "$config" > "$tmp"
  else
    { cat "$config"; printf '%s=%s\n' "$key" "$value"; } > "$tmp"
  fi
  chmod 600 "$tmp"
  mv "$tmp" "$config"
}

# HTTP status of GET $1/v1/context with bearer $2 (000 when the server cannot be reached).
http_status() { curl -s -o /dev/null -m 10 -w '%{http_code}' -H "Authorization: Bearer $2" "$1/v1/context" || true; }

# 0: the token works; 1: the server rejected it; 2: the server could not be asked.
token_check() {
  case $(http_status "$1" "$2") in
    2??|3??) return 0 ;;
    401|403) return 1 ;;
    *) return 2 ;;
  esac
}

normalize_url() {
  local u=${1%/}
  case $u in http://*|https://*) printf '%s' "$u" ;; *) die "the URL must start with http:// or https:// (got '$1')" ;; esac
}

# Adds ($1 = add) or removes ($1 = remove) the allow rule in the Claude Code user settings. Other keys are kept.
# Exit 0 done, 3 the file is not the JSON this expects, 4 neither python3 nor jq is installed.
edit_allow_rule() {
  local action=$1 allow_rule=$2 tmp
  mkdir -p "$(dirname "$claude_settings")"
  if have python3; then
    python3 -I - "$claude_settings" "$action" "$allow_rule" <<'PY'
import json, os, sys, tempfile
path, action, rule = sys.argv[1:4]
try:
    data = json.load(open(path)) if os.path.exists(path) and os.path.getsize(path) else {}
    if not isinstance(data, dict):
        raise ValueError
    perms = data.get("permissions", {})
    if not isinstance(perms, dict):
        raise ValueError
    allow = perms.get("allow", [])
    if not isinstance(allow, list):
        raise ValueError
except ValueError:
    sys.exit(3)
if action == "add" and rule not in allow:
    data.setdefault("permissions", perms)["allow"] = allow + [rule]
elif action == "remove" and rule in allow:
    allow.remove(rule)
else:
    sys.exit(0)
fd, tmp = tempfile.mkstemp(dir=os.path.dirname(path), prefix=".settings.")
with os.fdopen(fd, "w") as f:
    json.dump(data, f, indent=2)
    f.write("\n")
if os.path.exists(path):
    os.chmod(tmp, os.stat(path).st_mode & 0o777)
os.replace(tmp, path)
PY
  elif have jq; then
    [ -s "$claude_settings" ] || echo '{}' > "$claude_settings"
    tmp=$(mktemp "$(dirname "$claude_settings")/.settings.XXXXXX")
    if [ "$action" = add ]; then
      jq --arg r "$allow_rule" '.permissions.allow = ((.permissions.allow // []) + [$r] | unique)' "$claude_settings" > "$tmp" || { rm -f "$tmp"; return 3; }
    else
      jq --arg r "$allow_rule" 'if .permissions.allow then .permissions.allow -= [$r] else . end' "$claude_settings" > "$tmp" || { rm -f "$tmp"; return 3; }
    fi
    mv "$tmp" "$claude_settings"
  else
    return 4
  fi
}

allow_rule_step() {
  local action=$1 allow_rule=$2 rc=0
  edit_allow_rule "$action" "$allow_rule" || rc=$?
  case $rc in
    0) if [ "$action" = add ]; then note "allowed $allow_rule without prompts"; else note "cleared the $allow_rule allow rule"; fi ;;
    3) echo "warning: $claude_settings is not the JSON object I expected; edit it by hand: permissions.allow $action \"$allow_rule\"" >&2 ;;
    *) echo "warning: python3 or jq is needed to edit $claude_settings; edit it by hand: permissions.allow $action \"$allow_rule\"" >&2 ;;
  esac
}

cmd_connect() {
  local url="" token="" device="" injection=""
  while [ $# -gt 0 ]; do
    case $1 in
      --url) url=${2:?--url needs a value}; shift 2 ;;
      --token) token=${2:?--token needs a value}; shift 2 ;;
      --device) device=${2:?--device needs a value}; shift 2 ;;
      --injection) injection=${2:?--injection needs a value}; shift 2 ;;
      *) die "unknown option '$1'" ;;
    esac
  done
  [ -n "$url" ] || die "--url is required"
  case $injection in ""|lean|full) ;; *) die "--injection must be lean or full (got '$injection')" ;; esac
  url=$(normalize_url "$url")
  [ -n "$token" ] || token=$(cfg_get MEMORY_TOKEN)
  [ -n "$token" ] || die "no token: pass --token (or put MEMORY_TOKEN in $config)"
  case $token in mem_*_*) ;; *) die "that does not look like a MemoryVault token (expected mem_<prefix>_<secret>)" ;; esac
  [ -n "$device" ] || device=$(cfg_get DEVICE_ID)
  [ -n "$device" ] || device=$(hostname -s 2>/dev/null || hostname)
  have claude || die "the claude CLI is not on PATH"
  have curl || die "curl is needed to check the server"

  echo "Connecting Claude Code to $url as $device"
  local rc=0
  token_check "$url" "$token" || rc=$?
  case $rc in
    0) ;;
    1) die "$url rejected the token (revoked, mistyped, or minted by another server)" ;;
    *) die "cannot reach $url (is the server running, and is the URL right?)" ;;
  esac
  note "server accepts the token"

  MEM_INSTALL_QUIET=1 "$here/install.sh" >/dev/null
  note "built $here/bin/mem-hook"

  cfg_set MEMORY_URL "$url"
  cfg_set MEMORY_TOKEN "$token"
  cfg_set DEVICE_ID "$device"
  [ -z "$injection" ] || cfg_set MEM_INJECTION "$injection"
  note "wrote $config"

  # Plugin: removed first so that a moved checkout or a new version is picked up.
  claude plugin uninstall "$plugin@$marketplace" >/dev/null 2>&1 || true
  claude plugin marketplace remove "$marketplace" >/dev/null 2>&1 || true
  claude plugin marketplace add "$repo" >/dev/null || die "claude plugin marketplace add $repo failed"
  claude plugin install "$plugin@$marketplace" >/dev/null || die "claude plugin install $plugin@$marketplace failed"
  note "installed the plugin $plugin@$marketplace: hooks, skills and the MCP server (loads from $here)"

  # MCP: the plugin bundles the server (.mcp.json) and reads the URL and token from its own settings, so apps
  # that do not read the shell profile work too. The token goes to the keychain, not into ~/.claude.json.
  case $url$token in *\"*|*\\*) die "the URL and token must not contain quotes or backslashes" ;; esac
  printf '{"memory_url":"%s","memory_token":"%s"}' "$url" "$token" \
    | claude plugin configure "$plugin@$marketplace" --values-stdin >/dev/null \
    || die "claude plugin configure $plugin@$marketplace failed"
  claude mcp remove -s user memory >/dev/null 2>&1 && note "removed the old user-scope MCP entry 'memory'" || true
  note "configured the plugin's MCP server (token kept out of ~/.claude.json)"

  allow_rule_step remove "$legacy_allow_rule" >/dev/null
  allow_rule_step add "$allow_rule"

  echo
  echo "Done. Start a new Claude Code session (running ones keep the old setup)."
  echo "Check it with:  make connect-status    Undo it with:  make disconnect"
}

cmd_disconnect() {
  local purge=0
  while [ $# -gt 0 ]; do
    case $1 in
      --purge) purge=1; shift ;;
      *) die "unknown option '$1'" ;;
    esac
  done
  echo "Disconnecting Claude Code from MemoryVault"
  if have claude; then
    claude mcp remove -s user memory >/dev/null 2>&1 && note "removed the old user-scope MCP entry 'memory'" || true
    claude plugin uninstall "$plugin@$marketplace" >/dev/null 2>&1 && note "uninstalled the plugin (hooks, skills, MCP server and its saved token)" || note "the plugin was not installed"
    claude plugin marketplace remove "$marketplace" >/dev/null 2>&1 || true
  else
    echo "warning: the claude CLI is not on PATH; the MCP server and the plugin were left alone" >&2
  fi
  if [ -f "$claude_settings" ]; then
    allow_rule_step remove "$allow_rule"
    allow_rule_step remove "$legacy_allow_rule" >/dev/null
  fi
  if [ "$purge" = 1 ]; then
    rm -rf "$memory_home"
    note "deleted $memory_home"
  elif [ -f "$config" ]; then
    cfg_set MEMORY_URL ""
    cfg_set MEMORY_TOKEN ""
    note "blanked MEMORY_URL and MEMORY_TOKEN in $config (spool and sessions kept; --purge deletes them)"
  fi
  echo
  echo "Done."
}

cmd_probe() {
  local url="" token=""
  while [ $# -gt 0 ]; do
    case $1 in
      --url) url=${2:?--url needs a value}; shift 2 ;;
      --token) token=${2:?--token needs a value}; shift 2 ;;
      *) die "unknown option '$1'" ;;
    esac
  done
  local cfg_url
  cfg_url=$(cfg_get MEMORY_URL)
  if [ -z "$token" ]; then
    # A token saved for another server is of no use here.
    [ -z "$url" ] || [ "$url" = "$cfg_url" ] || return 1
    token=$(cfg_get MEMORY_TOKEN)
  fi
  [ -n "$url" ] || url=$cfg_url
  [ -n "$url" ] && [ -n "$token" ] || return 1
  token_check "$(normalize_url "$url")" "$token"
}

# Tokens served by this user's recent context and recall calls (GET $1/v1/stats/recall with bearer $2), one
# line per call kind and injection mode. Prints nothing when the server cannot be asked.
print_recall_stats() {
  local body days rows
  body=$(curl -fsS -m 10 -H "Authorization: Bearer $2" "$1/v1/stats/recall" 2>/dev/null) || return 0
  days=$(printf '%s' "$body" | sed -n 's/.*"days":\([0-9]*\).*/\1/p')
  rows=$(printf '%s' "$body" | sed 's/},{/}\n{/g' | sed -n 's/.*"mode":"\([a-z]*\)","injection":\(null\|"[a-z]*"\),"calls":\([0-9]*\),"avg_tokens":\([0-9.e+-]*\),"p50_tokens":\([0-9.e+-]*\),"p95_tokens":\([0-9.e+-]*\).*/\1 \2 \3 \4 \5 \6/p')
  if [ -z "$rows" ]; then
    echo "tokens   no context or recall calls in the last ${days:-7} days"
    return 0
  fi
  echo "tokens   served per call over the last ${days:-7} days (avg / p50 / p95)"
  printf '%s\n' "$rows" | awk '{ gsub(/"/, "", $2); if ($2 == "null") $2 = "-"
    printf "         %-8s %-5s %5d calls  %6.0f /%6.0f /%6.0f\n", $1, $2, $3, $4, $5, $6 }'
}

cmd_status() {
  local url token
  url=$(cfg_get MEMORY_URL)
  token=$(cfg_get MEMORY_TOKEN)
  if [ -z "$url" ] || [ -z "$token" ]; then
    echo "config   not connected ($config has no MEMORY_URL/MEMORY_TOKEN)"
  else
    echo "config   $url  device $(cfg_get DEVICE_ID)  token ${token%%_*}_$(printf '%s' "$token" | cut -d_ -f2)_..."
    local rc=0
    token_check "$url" "$token" || rc=$?
    case $rc in
      0) echo "server   reachable, token accepted" ;;
      1) echo "server   reachable, but the token was REJECTED (make connect-local or connect-remote again)" ;;
      *) echo "server   not reachable" ;;
    esac
    if [ $rc = 0 ]; then
      print_recall_stats "$url" "$token"
    fi
  fi
  local injection
  injection=$(cfg_get MEM_INJECTION)
  echo "inject   ${injection:-not set in the config: the per-user default on the server applies (full unless changed)}"
  if have claude; then
    if [ "$(claude plugin configure "$plugin@$marketplace" 2>/dev/null | grep -c ', set)')" = 2 ]; then
      echo "mcp      the plugin's MCP server has its URL and token"
    else
      echo "mcp      the plugin's MCP server is not configured"
    fi
    claude plugin list 2>/dev/null | grep -q "$plugin@$marketplace" && echo "plugin   $plugin@$marketplace installed" || echo "plugin   not installed"
  fi
  if [ -f "$claude_settings" ] && grep -q "mcp__plugin_memory_memory__" "$claude_settings"; then
    echo "allow    $allow_rule"
  else
    echo "allow    no allow rule (tool calls will prompt)"
  fi
  [ -x "$here/bin/mem-hook" ] && echo "binary   $here/bin/mem-hook" || echo "binary   $here/bin/mem-hook missing"
}

sub=${1:-}
[ $# -eq 0 ] || shift
case $sub in
  connect) cmd_connect "$@" ;;
  disconnect) cmd_disconnect "$@" ;;
  probe) cmd_probe "$@" ;;
  status) cmd_status ;;
  *) sed -n '2,16p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; [ -z "$sub" ] || [ "$sub" = help ] || exit 2 ;;
esac
