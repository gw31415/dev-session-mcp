#!/usr/bin/env bash
# Idempotent Cloudflare Tunnel + Access setup for the dev-session-mcp HTTP frontend.
#
# Creates or updates, in an existing Cloudflare account / Zero Trust organization:
#   1. a remotely-managed tunnel routing MCP_HOSTNAME -> http://127.0.0.1:8808
#   2. a proxied CNAME MCP_HOSTNAME -> <tunnel>.cfargotunnel.com
#   3. a reusable Access allow policy (existing Access group and/or emails only)
#   4. a self-hosted Access application for MCP_HOSTNAME with Managed OAuth
#      (dynamic client registration) so MCP clients can log in through Access
# and writes the tunnel token to TOKEN_FILE (mode 0600). Nothing is deleted.
#
# Required environment:
#   CLOUDFLARE_API_TOKEN  token with Account: Cloudflare Tunnel Edit,
#                         Account: Access: Apps and Policies Edit,
#                         Account: Access: Organizations, Identity Providers, and Groups Read,
#                         Zone: DNS Edit (for the zone of MCP_HOSTNAME)
#   CLOUDFLARE_ACCOUNT_ID
#   MCP_HOSTNAME          e.g. mcp.example.com (its zone must be in the account)
# Who may connect (at least one must be non-empty):
#   ALLOW_GROUP           existing Access group name (default: プロジェクトオーナー;
#                         set ALLOW_GROUP= to use emails only)
#   ALLOW_EMAILS          comma-separated emails (optional)
# Optional:
#   TUNNEL_NAME (dev-session-mcp), ORIGIN (http://127.0.0.1:8808),
#   APP_NAME (dev-session-mcp), SESSION_DURATION (24h),
#   REDIRECT_URIS (comma-separated OAuth redirect URIs allowed for DCR, besides
#                  localhost/loopback; defaults to Claude's connector callbacks),
#   TOKEN_FILE (./dev-session-mcp.tunnel-token), DRY_RUN=1 (read-only preview).
set -euo pipefail

: "${CLOUDFLARE_API_TOKEN:?set CLOUDFLARE_API_TOKEN}"
: "${CLOUDFLARE_ACCOUNT_ID:?set CLOUDFLARE_ACCOUNT_ID}"
: "${MCP_HOSTNAME:?set MCP_HOSTNAME, e.g. mcp.example.com}"
ALLOW_GROUP=${ALLOW_GROUP-プロジェクトオーナー}
if [ -z "$ALLOW_GROUP" ] && [ -z "${ALLOW_EMAILS:-}" ]; then
  echo "set ALLOW_GROUP (existing Access group name) and/or ALLOW_EMAILS" >&2
  exit 64
fi
TUNNEL_NAME=${TUNNEL_NAME:-dev-session-mcp}
ORIGIN=${ORIGIN:-http://127.0.0.1:8808}
APP_NAME=${APP_NAME:-dev-session-mcp}
POLICY_NAME="${APP_NAME} owners"
SESSION_DURATION=${SESSION_DURATION:-24h}
REDIRECT_URIS=${REDIRECT_URIS:-https://claude.ai/api/mcp/auth_callback,https://claude.com/api/mcp/auth_callback}
TOKEN_FILE=${TOKEN_FILE:-./dev-session-mcp.tunnel-token}
API=${CF_API_BASE:-https://api.cloudflare.com/client/v4}
ACCOUNT="$API/accounts/$CLOUDFLARE_ACCOUNT_ID"

log() { printf '%s\n' "$*" >&2; }

# api METHOD URL [JSON]: prints .result; fails on success:false.
api() {
  local method=$1 url=$2 body=${3:-} response
  if [ "${DRY_RUN:-}" = 1 ] && [ "$method" != GET ]; then
    log "DRY_RUN: $method ${url#"$API"} $(printf '%s' "$body" | jq -c . 2>/dev/null || true)"
    echo 'null'
    return
  fi
  local args=(-sS -X "$method" "$url" -H "Authorization: Bearer $CLOUDFLARE_API_TOKEN" -H 'Content-Type: application/json')
  [ -n "$body" ] && args+=(--data "$body")
  response=$(curl "${args[@]}")
  if [ "$(printf '%s' "$response" | jq -r '.success')" != true ]; then
    log "Cloudflare API error: $method ${url#"$API"}"
    printf '%s' "$response" | jq -c '.errors' >&2
    exit 1
  fi
  printf '%s' "$response" | jq -c '.result'
}

# --- Zone: longest suffix of MCP_HOSTNAME that is a zone in this account.
zone_id=
candidate=$MCP_HOSTNAME
while [ -z "$zone_id" ] && [[ $candidate == *.* ]]; do
  zone_id=$(api GET "$API/zones?name=$candidate&account.id=$CLOUDFLARE_ACCOUNT_ID" | jq -r '.[0].id // empty')
  [ -n "$zone_id" ] && log "zone: $candidate"
  candidate=${candidate#*.}
done
[ -n "$zone_id" ] || { log "no zone for $MCP_HOSTNAME in this account"; exit 1; }

# --- Tunnel (remotely managed).
tunnel_id=$(api GET "$ACCOUNT/cfd_tunnel?name=$TUNNEL_NAME&is_deleted=false" | jq -r '.[0].id // empty')
if [ -z "$tunnel_id" ]; then
  tunnel_id=$(api POST "$ACCOUNT/cfd_tunnel" "$(jq -nc --arg n "$TUNNEL_NAME" '{name:$n,config_src:"cloudflare"}')" | jq -r '.id // empty')
  log "tunnel created: ${tunnel_id:-<dry run>}"
else
  log "tunnel exists: $tunnel_id"
fi
if [ -n "$tunnel_id" ]; then
  api PUT "$ACCOUNT/cfd_tunnel/$tunnel_id/configurations" "$(jq -nc --arg h "$MCP_HOSTNAME" --arg o "$ORIGIN" \
    '{config:{ingress:[{hostname:$h,service:$o},{service:"http_status:404"}]}}')" >/dev/null
  log "tunnel ingress: $MCP_HOSTNAME -> $ORIGIN"
fi

# --- DNS.
target="${tunnel_id:-TUNNEL_ID}.cfargotunnel.com"
record=$(api GET "$API/zones/$zone_id/dns_records?name=$MCP_HOSTNAME" | jq -c '.[0] // empty')
dns=$(jq -nc --arg h "$MCP_HOSTNAME" --arg t "$target" '{type:"CNAME",name:$h,content:$t,proxied:true,comment:"dev-session-mcp tunnel"}')
if [ -z "$record" ]; then
  api POST "$API/zones/$zone_id/dns_records" "$dns" >/dev/null
  log "dns created: $MCP_HOSTNAME CNAME $target"
elif [ "$(printf '%s' "$record" | jq -r '.type + " " + .content')" = "CNAME $target" ]; then
  log "dns ok: $MCP_HOSTNAME CNAME $target"
elif [ "$(printf '%s' "$record" | jq -r '.type')" = CNAME ]; then
  api PUT "$API/zones/$zone_id/dns_records/$(printf '%s' "$record" | jq -r .id)" "$dns" >/dev/null
  log "dns updated: $MCP_HOSTNAME CNAME $target"
else
  log "refusing to replace existing $(printf '%s' "$record" | jq -r .type) record for $MCP_HOSTNAME"
  exit 1
fi

# --- Access policy: only the existing owners group and/or listed emails.
include='[]'
if [ -n "$ALLOW_GROUP" ]; then
  groups=$(api GET "$ACCOUNT/access/groups?per_page=1000")
  group_id=$(jq -r --arg n "$ALLOW_GROUP" 'map(select(.name == $n))[0].id // empty' <<<"$groups")
  if [ -z "$group_id" ]; then
    log "Access group not found: $ALLOW_GROUP. Existing groups:"
    jq -r '.[].name | "  " + .' <<<"$groups" >&2
    exit 1
  fi
  include=$(jq -c --arg g "$group_id" '. + [{group:{id:$g}}]' <<<"$include")
  log "allow group: $ALLOW_GROUP"
fi
if [ -n "${ALLOW_EMAILS:-}" ]; then
  include=$(jq -c --arg e "$ALLOW_EMAILS" '. + ($e | split(",") | map(gsub("^ +| +$";"")) | map(select(length>0)) | map({email:{email:.}}))' <<<"$include")
  log "allow emails: $(jq -r '[.[] | .email.email // empty] | length' <<<"$include") address(es)"
fi
policy=$(jq -nc --arg n "$POLICY_NAME" --argjson i "$include" '{name:$n,decision:"allow",include:$i,exclude:[],require:[]}')
policy_id=$(api GET "$ACCOUNT/access/policies?per_page=1000" | jq -r --arg n "$POLICY_NAME" 'map(select(.name == $n))[0].id // empty')
if [ -z "$policy_id" ]; then
  policy_id=$(api POST "$ACCOUNT/access/policies" "$policy" | jq -r '.id // empty')
  log "policy created: $POLICY_NAME"
else
  api PUT "$ACCOUNT/access/policies/$policy_id" "$policy" >/dev/null
  log "policy updated: $POLICY_NAME"
fi

# --- Access application with Managed OAuth for MCP clients.
oauth=$(jq -nc --arg r "$REDIRECT_URIS" '{enabled:true,dynamic_client_registration:{enabled:true,
  allow_any_on_localhost:true,allow_any_on_loopback:true,
  allowed_uris:($r | split(",") | map(gsub("^ +| +$";"")) | map(select(length>0)))}}')
desired=$(jq -nc --arg n "$APP_NAME" --arg h "$MCP_HOSTNAME" --arg d "$SESSION_DURATION" \
  --arg p "${policy_id:-POLICY_ID}" --argjson o "$oauth" \
  '{type:"self_hosted",name:$n,domain:$h,session_duration:$d,app_launcher_visible:false,
    auto_redirect_to_identity:false,policies:[{id:$p,precedence:1}],oauth_configuration:$o}')
app=$(api GET "$ACCOUNT/access/apps?per_page=1000" | jq -c --arg h "$MCP_HOSTNAME" 'map(select(.domain == $h))[0] // empty')
if [ -z "$app" ]; then
  api POST "$ACCOUNT/access/apps" "$desired" >/dev/null
  log "access app created: $APP_NAME ($MCP_HOSTNAME)"
else
  # PUT replaces the app: keep every existing field and override ours.
  merged=$(jq -c --argjson d "$desired" '. + $d | del(.id,.uid,.aud,.created_at,.updated_at)' <<<"$app")
  api PUT "$ACCOUNT/access/apps/$(jq -r .id <<<"$app")" "$merged" >/dev/null
  log "access app updated: $(jq -r .name <<<"$app") ($MCP_HOSTNAME); other policies on it were replaced by $POLICY_NAME"
fi

# --- Tunnel token for the VPS (never printed).
if [ -n "$tunnel_id" ] && [ "${DRY_RUN:-}" != 1 ]; then
  (umask 077 && api GET "$ACCOUNT/cfd_tunnel/$tunnel_id/token" | jq -r . >"$TOKEN_FILE")
  log "tunnel token written to $TOKEN_FILE (0600). Install it on the VPS as /etc/cloudflared/dev-session-mcp.token, then delete this copy."
fi

# --- Check that the hostname is not reachable without Access.
if [ "${DRY_RUN:-}" != 1 ]; then
  status=$(curl -sS -o /dev/null -w '%{http_code}' -X POST "https://$MCP_HOSTNAME/mcp" \
    -H 'Content-Type: application/json' --data '{}' || true)
  case "$status" in
    401|403|302) log "unauthenticated request rejected by Access (HTTP $status): OK" ;;
    530|502|000) log "HTTP $status: tunnel not connected yet (start cloudflared on the VPS) or DNS not propagated" ;;
    *) log "WARNING: unauthenticated request returned HTTP $status; check the Access application before connecting clients" ;;
  esac
fi
log "done. Connector URL: https://$MCP_HOSTNAME/mcp"
