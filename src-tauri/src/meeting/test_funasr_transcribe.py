import importlib.util
import os
import unittest
from pathlib import Path


MODULE_PATH = Path(__file__).with_name("funasr_transcribe.py")
SPEC = importlib.util.spec_from_file_location("funasr_transcribe", MODULE_PATH)
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class ParentWatchdogTests(unittest.TestCase):
    @unittest.skipUnless(os.name == "nt", "Windows process probe")
    def test_windows_process_probe_finds_current_process(self):
        self.assertTrue(MODULE.windows_process_is_alive(os.getpid()))

    def test_windows_tracks_snack_pid_instead_of_direct_parent(self):
        self.assertTrue(
            MODULE.parent_process_is_alive(
                42,
                platform="nt",
                direct_parent_pid=7,
                windows_pid_is_alive=lambda pid: pid == 42,
            )
        )

    def test_posix_requires_snack_to_remain_the_direct_parent(self):
        self.assertFalse(
            MODULE.parent_process_is_alive(
                42,
                platform="posix",
                direct_parent_pid=7,
                windows_pid_is_alive=lambda _pid: True,
            )
        )


if __name__ == "__main__":
    unittest.main()
