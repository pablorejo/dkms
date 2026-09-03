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
        cfg = render("orr", ORR_BASE + "grpc_tls: false\ncontrol_tls: false\npeers:\n  orr_2: 2\n")
        self.assertIs(cfg["grpc_tls"], False)
        self.assertEqual(cfg["peers"], {"orr_2": 2})
        self.assertNotIn("grpc_tls", cfg["peers"])
        # Con grpc_tls apagado y control_tls apagado no debe salir [tls].
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


class OrrServedDkms(unittest.TestCase):
    """served_dkms (B1): la superficie de aplicación del ORR queda acotada a
    su DKMS. Derivado del node_id del DKMS de la MISMA node.yml si no se
    declara; explícito manda; escalar (array), antes de cualquier tabla."""

    def test_derived_from_the_nodes_own_dkms(self):
        # Una node.yml de institución lleva el dkms (node_id) y el orr juntos.
        cfg = render("orr", ORR_BASE + "node_id: dkms-3\npeers:\n  orr_2: 2\n")
        self.assertEqual(cfg["served_dkms"], ["dkms-3"])
        # Y nunca dentro de [peers] (la clase de bug que fija este arnés).
        self.assertNotIn("served_dkms", cfg["peers"])

    def test_explicit_list_wins_over_derivation(self):
        cfg = render(
            "orr",
            ORR_BASE + "node_id: dkms-3\nserved_dkms: [dkms-7, dkms-8]\n",
        )
        self.assertEqual(cfg["served_dkms"], ["dkms-7", "dkms-8"])

    def test_without_dkms_nothing_is_emitted(self):
        # ORR suelto (sin dkms en la node.yml): no se inventa nada; el ORR
        # avisa al primer uso de la superficie abierta.
        cfg = render("orr", ORR_BASE)
        self.assertNotIn("served_dkms", cfg)


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


class QudittoTls(unittest.TestCase):
    """El ETSI-014 de quditto va con mTLS POR DEFECTO (sirve los pads OTP);
    `tls: false` es el opt-out explícito del caso sidecar."""

    def _env(self, node_yaml):
        text = render_raw("quditto", node_yaml)
        out = {}
        for line in text.splitlines():
            k, _, v = line.removeprefix("export ").partition("=")
            out[k] = v
        return out

    def test_default_is_mtls_with_cert_paths(self):
        env = self._env("r0: 2000\n")
        self.assertEqual(env["QUDITTO_TLS"], "on")
        self.assertEqual(env["QUDITTO_TLS_CERT"], "/config/certs/quditto.crt")
        self.assertEqual(env["QUDITTO_TLS_KEY"], "/config/certs/quditto.key")
        self.assertEqual(env["QUDITTO_TLS_CLIENT_CA"], "/config/certs/net-ca.crt")

    def test_cert_name_is_honored(self):
        env = self._env("r0: 2000\ncert_name: quditto-3\n")
        self.assertEqual(env["QUDITTO_TLS_CERT"], "/config/certs/quditto-3.crt")

    def test_explicit_opt_out(self):
        env = self._env("r0: 2000\ntls: false\n")
        self.assertEqual(env["QUDITTO_TLS"], "off")
        self.assertNotIn("QUDITTO_TLS_CERT", env)


DKMS_BASE = """\
node_id: dkms-1
advertise_ip: 10.0.0.7
sae_bindings: {sae_1: dkms-1}
"""


class ExplicitHotKeys(unittest.TestCase):
    def test_dkms_capacity_and_transport_e2e(self):
        cfg = render(
            "dkms",
            DKMS_BASE + "capacity_per_peer: 8192\n"
            "transport_e2e: {rekey_secs: 120, replay_window: 512}\n",
        )
        self.assertEqual(cfg["buffer"]["capacity_per_peer"], 8192)
        self.assertEqual(cfg["transport_e2e"]["rekey_secs"], 120)
        self.assertEqual(cfg["transport_e2e"]["replay_window"], 512)
        self.assertNotIn("suite", cfg["transport_e2e"])

    def test_orr_rotation_period_is_a_scalar_before_tables(self):
        cfg = render("orr", ORR_BASE + "rotation_period_ms: 120000\npeers:\n  orr_2: 2\n")
        self.assertEqual(cfg["rotation_period_ms"], 120000)
        self.assertNotIn("rotation_period_ms", cfg["peers"])


class ExtraFreeform(unittest.TestCase):
    """`extra:` — la válvula para campos de config.rs sin clave en node.yml."""

    def test_scalar_lands_before_the_first_table(self):
        cfg = render("orr", ORR_BASE + "peers: {orr_2: 2}\nextra:\n  deliver_queue_capacity: 1024\n")
        self.assertEqual(cfg["deliver_queue_capacity"], 1024)
        self.assertNotIn("deliver_queue_capacity", cfg["peers"])

    def test_dict_merges_into_an_emitted_table(self):
        cfg = render(
            "dkms",
            DKMS_BASE + "extra:\n  generator: {max_tokens_per_peer_per_tick: 64}\n",
        )
        g = cfg["generator"]
        self.assertEqual(g["max_tokens_per_peer_per_tick"], 64)
        # La tabla original sobrevive entera (el patrón sed de mesh.sh, bien).
        self.assertIn("ack_transport", g)

    def test_dict_appends_a_new_table(self):
        cfg = render("sdn", "extra:\n  rates: {num_fill_weight: 0.5}\n")
        self.assertEqual(cfg["rates"]["num_fill_weight"], 0.5)

    def test_new_table_after_links_array_stays_top_level(self):
        cfg = render(
            "qkc",
            "qkc_id: 1\nsdn_url: 10.0.0.2\n"
            "links:\n  - neighbor_id: 2\n"
            "extra:\n  foo: {bar: 1}\n",
        )
        self.assertEqual(cfg["foo"]["bar"], 1)
        self.assertNotIn("foo", cfg["links"][0])

    def test_collision_with_rendered_key_dies(self):
        with self.assertRaises(subprocess.CalledProcessError):
            render("orr", ORR_BASE + "extra:\n  orr_id: otro\n")
        with self.assertRaises(subprocess.CalledProcessError):
            render("dkms", DKMS_BASE + "extra:\n  generator: {ack_transport: socket}\n")

    def test_deep_nesting_dies(self):
        with self.assertRaises(subprocess.CalledProcessError):
            render("orr", ORR_BASE + "extra:\n  a: {b: {c: 1}}\n")


class QkcKmeCredentials(unittest.TestCase):
    """kme_cert/kme_key/kme_ca (A6): credencial POR ENLACE hacia ese KME (su
    PKI privada), rutas verbatim; sin declarar no se emite nada y el binario
    cae al [tls] de red (la prueba con quditto)."""

    QKD_LINK = (
        "qkc_id: 1\n"
        "links:\n"
        "  - neighbor_id: 2\n"
        "    type: qkd\n"
        "    neighbor_addr: 10.0.0.3\n"
        "    kme_url: kme-a.example:20010\n"
    )

    def test_per_link_kme_credentials_render_verbatim(self):
        cfg = render(
            "qkc",
            self.QKD_LINK
            + "    kme_cert: /mnt/kme-a/client.crt\n"
            + "    kme_key: /mnt/kme-a/client.key\n"
            + "    kme_ca: /mnt/kme-a/ca.crt\n",
        )
        lk = cfg["links"][0]
        self.assertEqual(lk["kme_cert"], "/mnt/kme-a/client.crt")
        self.assertEqual(lk["kme_key"], "/mnt/kme-a/client.key")
        self.assertEqual(lk["kme_ca"], "/mnt/kme-a/ca.crt")

    def test_without_declaration_nothing_is_emitted(self):
        lk = render("qkc", self.QKD_LINK)["links"][0]
        for k in ("kme_cert", "kme_key", "kme_ca"):
            self.assertNotIn(k, lk)

    def test_handshake_knobs_render_on_qkd_links_too(self):
        # A4 (opción B): el handshake también existe en enlaces qkd (raíz del
        # sello por-frame), así que pqc_auth / pqc_rekey_secs / pqc_suite
        # declarados NO pueden perderse dentro de la rama pqc — la misma clase
        # de bug que ya mordió con link_psk (2026-08-28). Y `off` SIN comillas
        # es booleano False en YAML 1.1: tiene que normalizarse al modo "off",
        # no renderizar "False" (que el binario rechaza al arrancar).
        cfg = render(
            "qkc",
            self.QKD_LINK
            + "    pqc_auth: off\n"
            + "    frame_auth: off\n"
            + "    pqc_rekey_secs: 120\n"
            + "    pqc_suite: ml-kem-1024\n",
        )
        lk = cfg["links"][0]
        self.assertEqual(lk["pqc_auth"], "off")
        self.assertEqual(lk["frame_auth"], "off")
        self.assertEqual(lk["pqc_rekey_secs"], 120)
        self.assertEqual(lk["pqc_suite"], "ml-kem-1024")
        self.assertEqual(lk["link_type"], "qkd")
        # `on`/`true` no es ningún modo: mejor morir nombrando los válidos.
        with self.assertRaises(subprocess.CalledProcessError):
            render("qkc", self.QKD_LINK + "    pqc_auth: on\n")


class SdnControlTls(unittest.TestCase):
    def test_cert_name_names_the_tls_paths(self):
        cfg = render("sdn", "control_tls: true\ncert_name: sdn-madrid\n")
        self.assertEqual(cfg["tls"]["cert_path"], "/config/certs/sdn-madrid.crt")
        self.assertEqual(cfg["tls"]["key_path"], "/config/certs/sdn-madrid.key")
        self.assertEqual(cfg["tls"]["client_ca"], "/config/certs/net-ca.crt")

    def test_default_cert_name_is_sdn(self):
        cfg = render("sdn", "control_tls: true\n")
        self.assertEqual(cfg["tls"]["cert_path"], "/config/certs/sdn.crt")

    def test_defaults_are_not_hardcoded_over_the_binary(self):
        # El render ya no pisa mcf_period_ms/push_debounce_ms (canónicos en
        # el binario: 5000/100) ni emite el default_policy muerto.
        cfg = render("sdn", "listen_ip: 0.0.0.0\n")
        self.assertNotIn("mcf_period_ms", cfg)
        self.assertNotIn("push_debounce_ms", cfg)
        self.assertNotIn("default_policy", cfg)
        cfg = render("sdn", "mcf_period_ms: 2500\npush_debounce_ms: 50\n")
        self.assertEqual(cfg["mcf_period_ms"], 2500)
        self.assertEqual(cfg["push_debounce_ms"], 50)

    def test_read_only_mirror_binds_to_loopback_by_default(self):
        cfg = render("sdn", "listen_ip: 0.0.0.0\nhttp_ro_port: 19003\n")
        self.assertEqual(cfg["http_ro_addr"], "127.0.0.1:19003")
        cfg = render("sdn", "http_ro_port: 19003\nhttp_ro_bind: 10.0.0.1\n")
        self.assertEqual(cfg["http_ro_addr"], "10.0.0.1:19003")

    def test_without_control_tls_no_tls_table(self):
        self.assertNotIn("tls", render("sdn", "control_tls: false\nmcf_period_ms: 5000\n"))


class ControlTlsDefaultOn(unittest.TestCase):
    """F1 (auditoría 2026-09-02): el plano de control va en mTLS DE FÁBRICA.
    Sin `control_tls` en node.yml la SDN y el QKC llevan [tls], y las URLs
    hacia la SDN sin esquema salen https; `control_tls: false` es el opt-out
    (todo http, sin [tls] salvo la identidad que grpc_tls exige al ORR); un
    esquema explícito en la URL manda siempre."""

    def test_sdn_has_tls_by_default(self):
        cfg = render("sdn", "listen_ip: 0.0.0.0\n")
        self.assertEqual(cfg["tls"]["cert_path"], "/config/certs/sdn.crt")
        self.assertEqual(cfg["tls"]["client_ca"], "/config/certs/net-ca.crt")

    def test_qkc_default_tls_and_https_announce(self):
        cfg = render("qkc", "qkc_id: 3\nsdn_url: 10.0.0.100\nadvertise_ip: 10.0.0.3\n")
        self.assertEqual(cfg["sdn_url"], "https://10.0.0.100:19002")
        self.assertEqual(cfg["tls"]["cert_path"], "/config/certs/qkc-3.crt")
        self.assertEqual(cfg["tls"]["control_plane_ca"], "/config/certs/net-ca.crt")

    def test_qkc_opt_out_is_plaintext(self):
        cfg = render("qkc", "qkc_id: 3\ncontrol_tls: false\nsdn_url: 10.0.0.100\n")
        self.assertEqual(cfg["sdn_url"], "http://10.0.0.100:19002")
        self.assertNotIn("tls", cfg)

    def test_explicit_scheme_wins(self):
        cfg = render("qkc", "qkc_id: 3\nsdn_url: http://10.0.0.100\n")
        self.assertEqual(cfg["sdn_url"], "http://10.0.0.100:19002")
        cfg = render("qkc", "qkc_id: 3\ncontrol_tls: false\nsdn_url: https://10.0.0.100\n")
        self.assertEqual(cfg["sdn_url"], "https://10.0.0.100:19002")

    def test_orr_and_dkms_follow_control_tls(self):
        cfg = render("orr", "orr_id: orr_1\nqkc_id: 1\nsdn_url: 10.0.0.2\n")
        self.assertEqual(cfg["sdn_url"], "https://10.0.0.2:19000")
        self.assertEqual(cfg["sdn_http_url"], "https://10.0.0.2:19002")
        cfg = render("orr", "orr_id: orr_1\nqkc_id: 1\ncontrol_tls: false\nsdn_url: 10.0.0.2\n")
        self.assertEqual(cfg["sdn_url"], "http://10.0.0.2:19000")
        self.assertEqual(cfg["sdn_http_url"], "http://10.0.0.2:19002")
        cfg = render("dkms", DKMS_BASE + "sdn_endpoint: 10.0.0.2\n")
        self.assertEqual(cfg["southbound"]["sdn_endpoint"], "https://10.0.0.2:19000")
        self.assertEqual(cfg["southbound"]["sdn_http_url"], "https://10.0.0.2:19002")
        cfg = render("dkms", DKMS_BASE + "sdn_endpoint: http://10.0.0.2:19000\n")
        self.assertEqual(cfg["southbound"]["sdn_endpoint"], "http://10.0.0.2:19000")
        self.assertEqual(cfg["southbound"]["sdn_http_url"], "http://10.0.0.2:19002")


class QkcLocalPlaneBind(unittest.TestCase):
    """D-14: el plano ORR↔QKC (20001, sin auth) va a loopback salvo
    `local_bind` explícito; con network_mode: host, 0.0.0.0 era todas las
    interfaces."""

    def test_local_listen_defaults_to_loopback(self):
        cfg = render("qkc", "qkc_id: 1\nlisten_ip: 0.0.0.0\n")
        self.assertEqual(cfg["local_listen"], "127.0.0.1:20001")
        self.assertEqual(cfg["peer_listen"], "0.0.0.0:20000")

    def test_local_bind_opens_it_on_purpose(self):
        cfg = render("qkc", "qkc_id: 1\nlocal_bind: 10.0.0.5\n")
        self.assertEqual(cfg["local_listen"], "10.0.0.5:20001")


class RendererQuoting(unittest.TestCase):
    """D-17/D-18: claves de tabla entrecomilladas y env del quditto
    escapado para el shell."""

    def test_orr_table_keys_are_quoted(self):
        raw = render_raw("orr", ORR_BASE + "peers:\n  'orr_2]': 2\npeer_grpc_addrs:\n  'orr_2]': http://10.0.0.12:20003\n")
        self.assertIn('"orr_2]" = 2', raw)
        cfg = tomllib.loads(raw)
        self.assertEqual(cfg["peers"]["orr_2]"], 2)
        self.assertEqual(cfg["peer_grpc_addrs"]["orr_2]"], "http://10.0.0.12:20003")

    def test_dkms_peer_table_header_is_quoted(self):
        cfg = render("dkms", DKMS_BASE + "peers:\n  'dkms-2.x':\n    endpoint: 10.0.0.2\n")
        self.assertEqual(cfg["peers"]["dkms-2.x"]["endpoint"], "https://10.0.0.2:20006")

    def test_quditto_env_is_shell_quoted(self):
        with tempfile.TemporaryDirectory() as td:
            node = os.path.join(td, "node.yml")
            with open(node, "w") as f:
                f.write("listen_ip: 0.0.0.0\nr0: 2000\ncert_name: 'x; touch /tmp/pwned'\n")
            out = os.path.join(td, "cfg")
            subprocess.run([sys.executable, RENDER, "quditto", node, out], check=True,
                           capture_output=True)
            env = open(os.path.join(out, "quditto.env")).read()
            self.assertIn("QUDITTO_TLS_CERT='/config/certs/x; touch /tmp/pwned.crt'", env)


class DkmsAckDefaults(unittest.TestCase):
    """F2: los ACK salen por ETSI-020 (mTLS) y el socket en claro no escucha,
    de fábrica; `socket` + `ack_socket_listen: true` solo durante una
    migración mixta, escrito a propósito."""

    def test_defaults_are_etsi020_and_listener_off(self):
        g = render("dkms", DKMS_BASE)["generator"]
        self.assertEqual(g["ack_transport"], "etsi020")
        self.assertIs(g["ack_socket_listen"], False)

    def test_explicit_socket_is_honoured(self):
        g = render("dkms", DKMS_BASE + "ack_transport: socket\nack_socket_listen: true\n")["generator"]
        self.assertEqual(g["ack_transport"], "socket")
        self.assertIs(g["ack_socket_listen"], True)


if __name__ == "__main__":
    unittest.main()
