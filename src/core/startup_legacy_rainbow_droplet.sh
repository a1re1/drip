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
# Color is fine: escapes that move the cursor, clear the screen or set a title
# are stripped before the banner is shown; colors pass through.

# One styled fragment: sgr 1;36 'text'.
sgr() {
  printf '\033[%sm%s\033[0m' "$1" "$2"
}

# Paint one line through a blue -> cyan -> violet ramp.
rainbow() {
  printf '%s' "$1" | awk 'BEGIN { n = split("45 51 81 111 147 183 219", ramp); cur = "" }
  {
    len = split($0, ch, "")
    for (i = 1; i <= len; i++) {
      if (ch[i] == " ") { printf " "; continue }
      code = ramp[int((i - 1) * n / len) + 1]
      if (code != cur) { printf "%c[38;5;%sm", 27, code; cur = code }
      printf "%s", ch[i]
    }
    printf "%c[0m\n", 27
  }'
}

rainbow '       __'
rainbow '      /  \'
rainbow '     /    \'
rainbow '     |    |'
rainbow '     \    /'
rainbow '      \  /'
rainbow '       \/'

active_model=$(printf '%s\n' "$DRIP_STARTUP_PROFILES" | grep '^\* ' | head -n 1)
active_model=${active_model#*: }
[ -n "$active_model" ] || active_model=$DRIP_STARTUP_ACTIVE_PROFILE
printf '  %s %s %s %s %s %s\n' \
  "$(sgr '1;36' '❯')" \
  "$(sgr '1;36' 'drip') $(sgr '38;5;45' "$DRIP_STARTUP_VERSION")" \
  "$(sgr '38;5;240' '·')" \
  "$(sgr '1;38;5;183' "$active_model")" \
  "$(sgr '38;5;240' '·')" \
  "$(sgr '38;5;250' "$DRIP_STARTUP_CWD")"
