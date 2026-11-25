"""Exercise the Habit predicate with an Agy-only home and a native checkpoint."""

import os
from pathlib import Path
import subprocess
import tempfile
import time
import unittest


class IngestSessionsDueTest(unittest.TestCase):
    def test_native_cadence_with_only_agy_sessions(self):
        script = Path(__file__).resolve().with_name("ingest-sessions.sh")
        with tempfile.TemporaryDirectory() as directory:
            home = Path(directory) / "home"
            state = Path(directory) / "state"
            transcript = home / (
                ".gemini/antigravity-cli/brain/fixture/"
                ".system_generated/logs/transcript_full.jsonl"
            )
            transcript.parent.mkdir(parents=True)
            transcript.write_text('{"source":"USER_INPUT","content":"fixture"}\n')
            checkpoint = state / "faculties/archive/ingest-sessions.json"
            checkpoint.parent.mkdir(parents=True)
            checkpoint.touch()
            command = ["bash", str(script), "--due"]
            env = {"HOME": str(home), "XDG_STATE_HOME": str(state), "PATH": "/usr/bin:/bin"}

            self.assertEqual(subprocess.run(command, env=env).returncode, 1)
            aged = time.time() - 21 * 3600
            os.utime(checkpoint, (aged, aged))
            self.assertEqual(subprocess.run(command, env=env).returncode, 0)


if __name__ == "__main__":
    unittest.main()
