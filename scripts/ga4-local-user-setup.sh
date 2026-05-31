#!/usr/bin/env bash
set -euo pipefail

SCOPES="https://www.googleapis.com/auth/analytics.readonly,https://www.googleapis.com/auth/cloud-platform"
ACCOUNT=""
CLIENT_ID_FILE=""
ENV_FILE=""
SERVICE="ga4-mcp-http.service"
RUN_LOGIN=1
RESTART_SERVICE=1
LOGIN_BROWSER_MODE="auto"

usage() {
  cat <<'EOF'
Usage: scripts/ga4-local-user-setup.sh [options]

Configure a local user-level GA4 MCP service for low-friction Google auth.

The script:
  1. Runs gcloud application-default login for the target Google user.
  2. Configures the service to use request tokens when present, otherwise ADC.
  3. Restarts the user systemd service when it exists.

Options:
  --account EMAIL          Google account to log in, e.g. user@example.com.
  --client-id-file PATH    Optional Google OAuth desktop client JSON.
  --env-file PATH          Service env file to update.
                           Default: discovered from systemd, then
                           ~/.config/ga4-mcp/ga4-mcp-http.env.
  --service NAME           User systemd service to restart.
                           Default: ga4-mcp-http.service
  --skip-login             Only update the service env file.
  --headless               Do not launch a browser; print a Google login URL
                           and wait for the auth code on this machine.
                           Alias for --no-launch-browser unless
                           --client-id-file is set; current gcloud requires
                           --no-browser for that combination.
  --no-launch-browser      Pass gcloud's --no-launch-browser flag.
  --no-browser             Pass gcloud's --no-browser remote-bootstrap flag.
                           This requires another machine with gcloud.
  --no-restart             Do not restart the user systemd service.
  -h, --help               Show this help.

This is intended for loopback/local user services. If the env file binds the
service to a non-loopback address without inbound auth enabled, the script
refuses to configure server-side credential fallback.
EOF
}

die() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

expand_user_path() {
  local path="$1"
  path="${path#-}"
  path="${path//%h/$HOME}"
  path="${path/#\~/$HOME}"
  printf '%s\n' "$path"
}

discover_env_file() {
  local service="$1"
  local unit
  if unit="$(systemctl --user cat "$service" 2>/dev/null)"; then
    local candidate
    candidate="$(
      printf '%s\n' "$unit" |
        awk -F= '
          $1 == "EnvironmentFile" {
            print $2
            exit
          }
        '
    )"
    if [[ -n "$candidate" ]]; then
      expand_user_path "$candidate"
      return 0
    fi
  fi

  if [[ -f "${HOME}/.config/ga4-mcp/ga4-mcp-http.env" ]]; then
    printf '%s\n' "${HOME}/.config/ga4-mcp/ga4-mcp-http.env"
  elif [[ -f "${HOME}/.config/ga4-mcp-http.env" ]]; then
    printf '%s\n' "${HOME}/.config/ga4-mcp-http.env"
  else
    printf '%s\n' "${HOME}/.config/ga4-mcp/ga4-mcp-http.env"
  fi
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --account)
      [[ $# -ge 2 ]] || die "--account requires a value"
      ACCOUNT="$2"
      shift 2
      ;;
    --client-id-file)
      [[ $# -ge 2 ]] || die "--client-id-file requires a value"
      CLIENT_ID_FILE="$2"
      shift 2
      ;;
    --env-file)
      [[ $# -ge 2 ]] || die "--env-file requires a value"
      ENV_FILE="$2"
      shift 2
      ;;
    --service)
      [[ $# -ge 2 ]] || die "--service requires a value"
      SERVICE="$2"
      shift 2
      ;;
    --skip-login)
      RUN_LOGIN=0
      shift
      ;;
    --headless|--no-launch-browser)
      [[ "$LOGIN_BROWSER_MODE" == "auto" || "$LOGIN_BROWSER_MODE" == "no-launch-browser" ]] \
        || die "--headless/--no-launch-browser cannot be combined with --no-browser"
      LOGIN_BROWSER_MODE="no-launch-browser"
      shift
      ;;
    --no-browser)
      [[ "$LOGIN_BROWSER_MODE" == "auto" || "$LOGIN_BROWSER_MODE" == "no-browser" ]] \
        || die "--no-browser cannot be combined with --headless/--no-launch-browser"
      LOGIN_BROWSER_MODE="no-browser"
      shift
      ;;
    --no-restart)
      RESTART_SERVICE=0
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      die "unknown option: $1"
      ;;
  esac
done

if [[ -z "$ENV_FILE" ]]; then
  ENV_FILE="$(discover_env_file "$SERVICE")"
fi

env_value() {
  local key="$1"
  local file="$2"
  awk -F= -v key="$key" '
    $0 ~ "^[[:space:]]*#" { next }
    $1 == key {
      value = substr($0, length($1) + 2)
      gsub(/^[[:space:]]+|[[:space:]]+$/, "", value)
      gsub(/^"|"$/, "", value)
      print value
      exit
    }
  ' "$file"
}

is_loopback_bind() {
  local bind="$1"
  case "$bind" in
    ""|127.*|localhost:*|localhost|::1|[[]::1[]]:*) return 0 ;;
    *) return 1 ;;
  esac
}

set_env_value() {
  local file="$1"
  local key="$2"
  local value="$3"
  local tmp
  tmp="$(mktemp "${file}.tmp.XXXXXX")"
  awk -v key="$key" -v value="$value" '
    BEGIN { written = 0 }
    $0 ~ "^[[:space:]]*#" || $0 !~ "^[A-Za-z_][A-Za-z0-9_]*=" {
      print
      next
    }
    {
      split($0, parts, "=")
      if (parts[1] == key) {
        print key "=" value
        written = 1
        next
      }
      print
    }
    END {
      if (!written) {
        print key "=" value
      }
    }
  ' "$file" > "$tmp"
  mv "$tmp" "$file"
  chmod 600 "$file"
}

if [[ "$RUN_LOGIN" -eq 1 ]]; then
  command -v gcloud >/dev/null 2>&1 || die "gcloud is required for ADC login"
  login_cmd=(gcloud auth application-default login)
  if [[ -n "$ACCOUNT" ]]; then
    login_cmd+=("$ACCOUNT")
  fi
  if [[ -n "$CLIENT_ID_FILE" ]]; then
    [[ -f "$CLIENT_ID_FILE" ]] || die "OAuth client JSON not found: $CLIENT_ID_FILE"
    login_cmd+=("--client-id-file=$CLIENT_ID_FILE")
  fi
  case "$LOGIN_BROWSER_MODE" in
    no-launch-browser)
      if [[ -n "$CLIENT_ID_FILE" ]]; then
        printf 'gcloud requires --no-browser when --client-id-file is set; using remote-bootstrap mode.\n' >&2
        login_cmd+=("--no-browser")
      else
        login_cmd+=("--no-launch-browser")
      fi
      ;;
    no-browser)
      login_cmd+=("--no-browser")
      ;;
  esac
  login_cmd+=("--scopes=$SCOPES")

  printf 'Running Google ADC login'
  if [[ -n "$ACCOUNT" ]]; then
    printf ' for %s' "$ACCOUNT"
  fi
  printf '...\n'
  "${login_cmd[@]}"
  gcloud auth application-default print-access-token >/dev/null
fi

mkdir -p "$(dirname "$ENV_FILE")"
if [[ ! -f "$ENV_FILE" ]]; then
  install -m 600 /dev/null "$ENV_FILE"
fi
chmod 600 "$ENV_FILE"

bind_addr="$(env_value GA4_MCP_BIND_ADDR "$ENV_FILE")"
auth_enabled="$(env_value GA4_MCP_AUTH_ENABLED "$ENV_FILE")"
bind_addr="${bind_addr:-127.0.0.1:9420}"
auth_enabled="${auth_enabled:-0}"

if ! is_loopback_bind "$bind_addr" && [[ "$auth_enabled" != "1" ]]; then
  die "refusing to enable server-side credential fallback for non-loopback bind '$bind_addr' without GA4_MCP_AUTH_ENABLED=1"
fi

backup="${ENV_FILE}.bak.$(date +%Y%m%d%H%M%S)"
cp "$ENV_FILE" "$backup"
chmod 600 "$backup"

set_env_value "$ENV_FILE" "GOOGLE_ANALYTICS_MCP_SCOPE" "https://www.googleapis.com/auth/analytics.readonly"
set_env_value "$ENV_FILE" "GOOGLE_ANALYTICS_MCP_UPSTREAM_TOKEN_SOURCE" "request_header_or_config"
set_env_value "$ENV_FILE" "GOOGLE_ANALYTICS_MCP_UPSTREAM_TOKEN_HEADER" "authorization"

printf 'Updated %s\n' "$ENV_FILE"
printf 'Backup written to %s\n' "$backup"

if [[ "$RESTART_SERVICE" -eq 1 ]]; then
  if systemctl --user status "$SERVICE" >/dev/null 2>&1; then
    systemctl --user restart "$SERVICE"
    printf 'Restarted user service %s\n' "$SERVICE"
  else
    printf 'User service %s was not found or not active; restart it manually if needed.\n' "$SERVICE"
  fi
fi

cat <<EOF

Local user auth is configured.

The service now accepts per-request Google bearer tokens when clients send them,
and otherwise falls back to the logged-in ADC identity.
EOF
