from __future__ import annotations

from pathlib import Path
from typing import Optional

from pydantic import model_validator

from .model import Model

try:
    from conf import resolve_rel
except Exception:  # pragma: no cover - fallback para entornos sin conf
    resolve_rel = None


def _resolve_path(path_value: str) -> Path:
    if resolve_rel is None:
        return Path(path_value)
    try:
        return resolve_rel(path_value)
    except Exception:
        return Path(path_value)


class ModelFile(Model):
    """Representación pydantic de la tabla data_file."""

    id: Optional[int] = None
    path: str
    data: Optional[str] = None

    @model_validator(mode="after")
    def _hydrate_data_from_path(self) -> "ModelFile":
        if self.data is not None:
            return self
        if not self.path:
            return self
        candidate = _resolve_path(self.path)
        if not candidate.is_file():
            return self
        try:
            self.data = candidate.read_text(encoding="utf-8")
        except UnicodeDecodeError:
            self.data = candidate.read_text(encoding="latin-1")
        except OSError:
            # Si no se puede leer el fichero, deja data en None.
            return self
        return self
