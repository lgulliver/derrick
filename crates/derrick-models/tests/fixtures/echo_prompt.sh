#!/bin/sh
input=$(cat)
prompt=$(printf '%s' "$input" | sed 's/.*"prompt":"\([^"]*\)".*/\1/')
printf '<<DERRICK-CONTENT>> %s\n' "$prompt"
printf '<<DERRICK-META>> {"tokens_in":7,"tokens_out":11,"finish_reason":"stop"}\n'
