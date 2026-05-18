# /speckit/clarify

Read `.specify/feature.json` to find the feature directory, then read `spec.md`
from that directory.

If there are **Open questions** in the spec, surface the most important 1–3 as
clarifying questions to the user and wait for answers. Update the spec with the
answers, removing the resolved open questions.

If there are no open questions, reply: `no open questions — spec is ready for planning`
and exit cleanly (the pipeline will skip this step).
