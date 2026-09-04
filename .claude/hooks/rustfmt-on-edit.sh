#!/usr/bin/env sh
file=$(jq -r '.tool_input.file_path // empty')
case "$file" in
  *.rs) rustfmt --edition 2024 "$file" 2>/dev/null ;;
esac
exit 0
