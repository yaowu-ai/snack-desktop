import importlib.util
import os
import sys
import types
import unittest
from pathlib import Path
from unittest.mock import Mock, patch


MODULE_PATH = Path(__file__).with_name("funasr_transcribe.py")
SPEC = importlib.util.spec_from_file_location("funasr_transcribe", MODULE_PATH)
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class SpeakerClusteringTests(unittest.TestCase):
    def setUp(self):
        self.cluster = Mock(return_value=[0, 1])
        self.model = types.SimpleNamespace(
            cb_model=types.SimpleNamespace(umap_hdbscan_cluster=self.cluster)
        )
        self.torch = types.SimpleNamespace(is_tensor=lambda value: value is self.tensor)
        self.tensor = Mock()
        self.numpy_values = object()
        self.tensor.detach.return_value.cpu.return_value.numpy.return_value = self.numpy_values

    def test_tensor_is_detached_and_moved_to_cpu_before_umap(self):
        with patch.dict(sys.modules, {"torch": self.torch}):
            MODULE.configure_speaker_clustering(self.model)
            result = self.model.cb_model.umap_hdbscan_cluster(self.tensor)

        self.tensor.detach.assert_called_once_with()
        self.tensor.detach.return_value.cpu.assert_called_once_with()
        self.tensor.detach.return_value.cpu.return_value.numpy.assert_called_once_with()
        self.cluster.assert_called_once_with(self.numpy_values)
        self.assertEqual(result, [0, 1])

    def test_numpy_input_is_passed_through_unchanged(self):
        with patch.dict(sys.modules, {"torch": self.torch}):
            MODULE.configure_speaker_clustering(self.model)
            self.model.cb_model.umap_hdbscan_cluster(self.numpy_values)

        self.cluster.assert_called_once_with(self.numpy_values)
        self.tensor.detach.assert_not_called()

    def test_model_is_configured_before_generate_and_keeps_speaker_model(self):
        self.model.generate = Mock(return_value=[{"text": "test"}])
        auto_model = Mock(return_value=self.model)
        configure = Mock()

        def generate(**_kwargs):
            configure.assert_called_once_with(self.model)
            return [{"text": "test"}]

        self.model.generate.side_effect = generate
        with (
            patch.dict(os.environ, {"MODELSCOPE_CACHE": "model-cache"}),
            patch.dict(sys.modules, {"funasr": types.SimpleNamespace(AutoModel=auto_model)}),
            patch.object(MODULE, "model_dir", side_effect=lambda cache, name: cache / name),
            patch.object(MODULE, "configure_speaker_clustering", configure),
        ):
            result = MODULE.run_model(Path("recording.wav"))

        self.assertEqual(result, [{"text": "test"}])
        self.assertIn("spk_model", auto_model.call_args.kwargs)
        self.model.generate.assert_called_once_with(
            input="recording.wav", batch_size_s=300, sentence_timestamp=True
        )


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
