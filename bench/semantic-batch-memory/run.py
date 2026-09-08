"""Run paired real-inference probes; sample Linux process memory, retain raw evidence."""
import argparse
import json
import pathlib
import subprocess
import time
import uuid

parser = argparse.ArgumentParser()
parser.add_argument("scratch", type=pathlib.Path)
parser.add_argument("--target", type=pathlib.Path, required=True)
parser.add_argument("--repeat", type=int, default=1)
parser.add_argument("--corpus", choices=["ordinary", "skewed"])
parser.add_argument("--mode", choices=["full", "watcher"])
args = parser.parse_args()
args.scratch = args.scratch.resolve()
args.target = args.target.resolve()
if args.repeat < 1:
    parser.error("--repeat must be positive")
results_path = args.scratch / "results.json"
results = json.loads(results_path.read_text()) if results_path.exists() else []
completed = {row["key"] for row in results}
for repetition in range(args.repeat):
    for corpus in ([args.corpus] if args.corpus else ["ordinary", "skewed"]):
        for mode in ([args.mode] if args.mode else ["full", "watcher"]):
            order = ["baseline", "budget"] if repetition % 2 == 0 else ["budget", "baseline"]
            for variant in order:
                key = f"{corpus}-{mode}-{variant}-{repetition}"
                if key in completed:
                    continue
                output = args.scratch / f"{key}.stdout"
                error = args.scratch / f"{key}.stderr"
                db = args.scratch / f"{key}-{uuid.uuid4().hex}.db"
                if output.exists() or error.exists():
                    artifact_key = key + f"-retry-{uuid.uuid4().hex[:8]}"
                    output = args.scratch / f"{artifact_key}.stdout"
                    error = args.scratch / f"{artifact_key}.stderr"
                command = [str(args.target / f"semantic-memory-{variant}"),
                           str(args.scratch / corpus), mode, str(db)]
                started = time.monotonic()
                with output.open("w") as out, error.open("w") as err:
                    process = subprocess.Popen(command, stdout=out, stderr=err)
                    peak_rss = peak_private = samples = 0
                    while process.poll() is None:
                        try:
                            values = {}
                            for line in pathlib.Path(f"/proc/{process.pid}/smaps_rollup").read_text().splitlines()[1:]:
                                fields = line.split()
                                values[fields[0].rstrip(":")] = int(fields[1])
                            peak_rss = max(peak_rss, values.get("Rss", 0))
                            peak_private = max(peak_private, values.get("Private_Clean", 0) + values.get("Private_Dirty", 0))
                            samples += 1
                        except (FileNotFoundError, ProcessLookupError):
                            pass
                        time.sleep(0.02)
                row = ({"returncode": process.returncode, "error": error.read_text()}
                       if process.returncode else json.loads(output.read_text().splitlines()[-1]))
                accounting = [json.loads(line.removeprefix("ACCOUNT ")) for line in error.read_text().splitlines() if line.startswith("ACCOUNT ")]
                row.update(key=key, database=str(db), wall_seconds=time.monotonic() - started,
                           sampled_peak_rss_kib=peak_rss, sampled_peak_private_kib=peak_private,
                           samples=samples, batches=accounting)
                results.append(row)
                results_path.write_text(json.dumps(results, indent=2) + "\n")
                print(json.dumps({k: v for k, v in row.items() if k != "batches"}), flush=True)

if any(row.get("returncode") for row in results):
    raise SystemExit(1)
