import os
from pathlib import Path
import json
import logging

# CONFIG_FOLDER puede inyectarse por entorno. Por defecto se resuelve
# al directorio "config_files" del árbol del repo.
_BASE_DIR = Path(__file__).resolve().parents[2]  # apunta a code_dkms/
_DEFAULT_CONFIG = str((_BASE_DIR / 'config_files').resolve())
CONFIG_FOLDER = os.environ.get('CONFIG_FOLDER', _DEFAULT_CONFIG)

TIME_OUT = int(os.getenv("QKC_SEND_TIMEOUT_SECONDS", "120"))

def resolve_rel(path_str: str) -> Path:
    """
    Resuelve rutas relativas tipo 'code_dkms/...' aunque estés ya dentro de 'code_dkms/'.
    - Si la ruta es absoluta, la devuelve tal cual.
    - Si la ruta empieza por el nombre del cwd, elimina ese primer componente para evitar
        'code_dkms/code_dkms/...'.
    - En cualquier otro caso, la resuelve respecto al cwd.
    """
    p = Path(path_str)
    if p.is_absolute():
        return p

    cwd = Path.cwd()
    parts = p.parts
    if parts and parts[0] == cwd.name:
        # Estás en .../code_dkms y la ruta es 'code_dkms/src/...'
        p = cwd.joinpath(*parts[1:])
    else:
        p = cwd / p

    return p.resolve()


def get_data_json(type, json_file):
    """
    Recupera un JSON de configuración localizando el fichero en distintas rutas:
    1) CONFIG_FOLDER/<json_file>
    2) CONFIG_FOLDER/<type>/<json_file>
    3) CONFIG_FOLDER/<type>/<basename(json_file)>
    4) CONFIG_FOLDER/<basename(json_file)>
    """
    candidates = [
        os.path.join(CONFIG_FOLDER, json_file),
        os.path.join(CONFIG_FOLDER, type, json_file),
        os.path.join(CONFIG_FOLDER, type, os.path.basename(json_file)),
        os.path.join(CONFIG_FOLDER, os.path.basename(json_file)),
    ]
    for path in candidates:
        if os.path.isfile(path):
            with open(path, "r", encoding="utf-8") as f:
                return json.load(f)
    raise FileNotFoundError(f"No existe el fichero de configuración en {candidates}")



def setup_logger(id, type_node) -> logging.Logger:
    log_dir = Path(os.environ.get("LOG_DIR", "/app/logs"))
    log_dir = Path.joinpath(log_dir, type_node)

    try:
        log_dir.mkdir(parents=True, exist_ok=True)
    except Exception:
        log_dir = Path("/tmp")
        log_dir.mkdir(parents=True, exist_ok=True)

    import re
    raw_id = id or "unknown"
    safe_id = re.sub(r"[^A-Za-z0-9._-]+", "_", str(raw_id))
    log_path = log_dir / f"{safe_id}.log"
    logger = logging.getLogger(f"node_{type_node}.{safe_id}")
    logger.setLevel(logging.DEBUG)

    # Define a custom formatter that always adds 'type_node - ' before the message
    class TypeNodeFormatter(logging.Formatter):
        def format(self, record):
            original_msg = record.getMessage()
            record.msg = f"{type_node} - {original_msg}"
            record.args = ()
            return super().format(record)

    if not any(isinstance(h, logging.FileHandler) and getattr(h, 'baseFilename', '') == str(log_path) for h in logger.handlers):
        handler = logging.FileHandler(log_path, encoding="utf-8")
        handler.setFormatter(TypeNodeFormatter("%(asctime)s %(levelname)s %(message)s"))
        logger.addHandler(handler)
    return logger


if __name__ == "__main__":
    # Quick self-check
    print("CONFIG_FOLDER:", CONFIG_FOLDER)
    print("Resolved conf path:", resolve_rel('code_dkms/src/conf/conf.py'))
