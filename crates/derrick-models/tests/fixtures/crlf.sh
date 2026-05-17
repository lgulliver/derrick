#!/bin/sh
cat >/dev/null
printf '<<DERRICK-CONTENT>> crlf one\r\n'
printf '<<DERRICK-META>> {"tokens_in":3,"tokens_out":4,"finish_reason":"stop"}\r\n'
