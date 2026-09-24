#!/bin/sh
# drip's startup message - printed once when an interactive session opens.
#
# Edit this file to change what drip shows at startup, or delete it for no
# banner at all. Facts about this run arrive as environment variables (the same
# fields also arrive as one JSON object on stdin):
#
#   DRIP_STARTUP_VERSION         drip's version, e.g. 0.1.0
#   DRIP_STARTUP_CWD             the directory this session runs in
#   DRIP_STARTUP_SESSION_ID      this session's id
#   DRIP_STARTUP_ACTIVE_PROFILE  the model profile chosen for startup
#   DRIP_STARTUP_PROFILES        one "id: model" line per configured profile,
#                                the active one marked with *
#
# Output is plain text: escape sequences are stripped before it is displayed.

cat <<'DRIP_MASCOT'
        .
       / \
      / . \
     |     |
      \   /
       \_/
DRIP_MASCOT

printf '  drip %s\n' "$DRIP_STARTUP_VERSION"
printf '  %s\n' "$DRIP_STARTUP_CWD"
printf '  session %s\n' "$DRIP_STARTUP_SESSION_ID"
printf '  model profiles (active: %s)\n' "$DRIP_STARTUP_ACTIVE_PROFILE"
printf '%s\n' "$DRIP_STARTUP_PROFILES" | while IFS= read -r profile; do
  if [ -n "$profile" ]; then
    printf '    %s\n' "$profile"
  fi
done
