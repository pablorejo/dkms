"""Tests del renderer node.yml -> TOML (render_config.py).

Corren contra el CLI real (subprocess), igual que lo usa entrypoint.sh, y
parsean la salida con tomllib: lo que se fija aquí es el TOML que el binario
va a cargar de verdad, no la lista de líneas intermedia.

Solo stdlib (unittest + tomllib, Python >= 3.11) además del PyYAML que ya
necesita el propio render_config.py. Se ejecutan con `make rendercheck`
(incluido en `make check`) y en CI.

El motivo de existir: en TOML una clave suelta después de una tabla pertenece
a esa tabla. Ya mordió dos veces con `grpc_tls` (dentro de [tls] primero,
dentro de [peers] después — peer fantasma y opt-out ignorado), y ningún test
lo cazaba porque mesh.sh no usa `peers:`.
"""

import os
import subprocess
import sys
import tempfile
import tomllib
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
RENDER = os.path.join(HERE, "render_config.py")

OUT_FILE = {
    "qkc": "qkc.toml",
    "orr": "default.toml",
    "dkms": "default.toml",
    "sdn": "default.toml",
    "quditto": "quditto.env",
}


def render_raw(role, node_yaml):
    """Invoca el CLI real y devuelve el texto del fichero rendericado."""
    with tempfile.TemporaryDirectory() as td:
        node = os.path.join(td, "node.yml")
        with open(node, "w") as f:
            f.write(node_yaml)
        out = os.path.join(td, "cfg")
        subprocess.run(
            [sys.executable, RENDER, role, node, out],
            check=True,
            capture_output=True,
        )
        with open(os.path.join(out, OUT_FILE[role])) as f:
            return f.read()


def render(role, node_yaml):
    """Como render_raw, pero parseado como TOML (roles con fichero TOML)."""
    return tomllib.loads(render_raw(role, node_yaml))


ORR_BASE = """\
orr_id: orr_1
qkc_id: 1
sdn_url: http://10.0.0.2:19000
"""


class OrrGrpcTlsPlacement(unittest.TestCase):
    """grpc_tls es un escalar: antes de CUALQUIER tabla ([peers], [tls])."""

    def test_opt_out_survives_a_peers_table(self):
        # La forma exacta de docker/examples/node.orr.yml: peers + grpc_tls.
        # Antes `grpc_tls` se emitía después de [peers] y acababa dentro de la
        # tabla: peer fantasma `grpc_tls -> 1` y el false ignorado.
        cfg = render("orr", ORR_BASE + "grpc_tls: false\npeers:\n  orr_2: 2\n")
        self.assertIs(cfg["grpc_tls"], False)
        self.assertEqual(cfg["peers"], {"orr_2": 2})
        self.assertNotIn("grpc_tls", cfg["peers"])
        # Con grpc_tls apagado y sin control_tls no debe salir [tls].
        self.assertNotIn("tls", cfg)

    def test_default_on_with_peers_keeps_tls_block(self):
        cfg = render("orr", ORR_BASE + "peers:\n  orr_2: 2\n  orr_3: 3\n")
        self.assertIs(cfg["grpc_tls"], True)
        self.assertEqual(cfg["peers"], {"orr_2": 2, "orr_3": 3})
        # grpc_tls fuerza la identidad de nodo aunque no haya control_tls.
        self.assertEqual(cfg["tls"]["cert_path"], "/config/certs/orr_1.crt")
        self.assertEqual(cfg["tls"]["key_path"], "/config/certs/orr_1.key")
        self.assertEqual(cfg["tls"]["control_plane_ca"], "/config/certs/net-ca.crt")
        # Y nunca dentro de [tls] (la primera mordida de esta clase de bug).
        self.assertNotIn("grpc_tls", cfg["tls"])

    def test_scalar_without_tables_still_renders(self):
        cfg = render("orr", ORR_BASE)
        self.assertIs(cfg["grpc_tls"], True)
        self.assertEqual(cfg["orr_id"], "orr_1")


class SdnHttpUrlScheme(unittest.TestCase):
    """sdn_http_url hereda el esquema del sdn_url: una SDN con control_tls se
    escribe https:// en cada node.yml y el anuncio sale mTLS. Antes se emitía
    http:// siempre y un despliegue con la SDN en mTLS anunciaba en claro
    contra un puerto TLS para siempre."""

    def test_https_sdn_url_derives_https_announce(self):
        cfg = render("orr", "orr_id: orr_1\nqkc_id: 1\nsdn_url: https://10.0.0.2:19000\n")
        self.assertEqual(cfg["sdn_http_url"], "https://10.0.0.2:19002")

    def test_http_sdn_url_stays_http(self):
        cfg = render("orr", ORR_BASE)
        self.assertEqual(cfg["sdn_http_url"], "http://10.0.0.2:19002")

    def test_dkms_inherits_the_scheme_too(self):
        cfg = render(
            "dkms",
            "node_id: dkms-1\nadvertise_ip: 10.0.0.7\n"
            "sdn_endpoint: https://10.0.0.2:19000\n"
            "sae_bindings: {sae_1: dkms-1}\n",
        )
        self.assertEqual(cfg["southbound"]["sdn_http_url"], "https://10.0.0.2:19002")


class SdnControlTls(unittest.TestCase):
    def test_cert_name_names_the_tls_paths(self):
        cfg = render("sdn", "control_tls: true\ncert_name: sdn-madrid\n")
        self.assertEqual(cfg["tls"]["cert_path"], "/config/certs/sdn-madrid.crt")
        self.assertEqual(cfg["tls"]["key_path"], "/config/certs/sdn-madrid.key")
        self.assertEqual(cfg["tls"]["client_ca"], "/config/certs/net-ca.crt")

    def test_default_cert_name_is_sdn(self):
        cfg = render("sdn", "control_tls: true\n")
        self.assertEqual(cfg["tls"]["cert_path"], "/config/certs/sdn.crt")

    def test_without_control_tls_no_tls_table(self):
        self.assertNotIn("tls", render("sdn", "mcf_period_ms: 5000\n"))


if __name__ == "__main__":
    unittest.main()
