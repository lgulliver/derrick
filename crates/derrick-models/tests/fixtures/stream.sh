#!/bin/sh
cat >/dev/null
printf '<<DERRICK-CONTENT>> one\n'
printf '<<DERRICK-CONTENT>> two\n'
printf '<<DERRICK-META>> {"tokens_in":2,"tokens_out":3,"finish_reason":"length"}\n'
