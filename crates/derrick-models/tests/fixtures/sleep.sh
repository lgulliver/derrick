#!/bin/sh
cat >/dev/null
sleep 2
printf '<<DERRICK-CONTENT>> too late\n'
printf '<<DERRICK-META>> {"tokens_in":1,"tokens_out":1,"finish_reason":"stop"}\n'
