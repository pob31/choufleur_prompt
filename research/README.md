# research/

Python sidecar for offline work: forced alignment to draft ground-truth labels,
and notebooks for poking at matching behaviour.

**Nothing here is ever in the show path.** No Rust crate imports it, no server
process runs it, and it is not needed to build or run Choufleur. It exists because
labelling an act by hand is a day's work and correcting an alignment is an hour.
The PRD is explicit about this boundary: Python is welcome as an offline research
harness, never in the show path.

```bash
python3 -m venv .venv
. .venv/bin/activate
pip install -r requirements.txt

python align.py ../corpus/<show>-<act>
```

See [`../corpus/README.md`](../corpus/README.md) for the labelling workflow this
fits into — the short version is: align, correct in Audacity, fold back.

`align.py` mirrors the §3.2 normalization from `choufleur-core` deliberately, so
alignment and matching are comparing the same strings. If that spec changes,
both implementations change together.
