import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location("prepare", Path(__file__).parents[1] / "scripts/prepare_hysteria.py")
prepare = importlib.util.module_from_spec(spec)
spec.loader.exec_module(prepare)


class ConversionTests(unittest.TestCase):
    def test_preserves_tls_auth_obfs_and_uses_only_loopback(self):
        node = {"server": "2001:db8::1", "port": 443, "password": "synthetic-secret",
                "sni": "example.test", "skip-cert-verify": True, "fingerprint": "ab:cd",
                "obfs": "salamander", "obfs-password": "synthetic-obfs", "alpn": ["h3"]}
        config = prepare.client_config(node, 19080)
        self.assertEqual(config["server"], "[2001:db8::1]:443")
        self.assertEqual(config["auth"], "synthetic-secret")
        self.assertEqual(config["tls"], {"sni": "example.test", "insecure": True, "pinSHA256": "ab:cd"})
        self.assertEqual(config["socks5"]["listen"], "127.0.0.1:19080")
        self.assertTrue(config["lazy"])
        self.assertEqual(config["obfs"]["salamander"]["password"], "synthetic-obfs")

    def test_rejects_unsupported_alpn(self):
        with self.assertRaises(ValueError):
            prepare.client_config({"server": "example.test", "alpn": ["wrong"]}, 19080)


if __name__ == "__main__":
    unittest.main()
