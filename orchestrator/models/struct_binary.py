from __future__ import annotations

import struct
from typing import Type, TypeVar, get_type_hints, Optional, Union

from pydantic import BaseModel

T = TypeVar("T", bound="Struct")


class Struct(BaseModel):
    """
    Clase base genérica que empaqueta/desempaqueta cualquier modelo Pydantic
    con campos simples (int, bool, float, str, bytes) y también otros Struct
    usando struct.
    """

    def to_bytes(self) -> bytes:
        """
        Serializa la instancia a un frame binario.
        Formato:
            - int        -> q (int64)
            - bool       -> ? (bool)
            - float      -> d (float64)
            - str        -> I + bytes UTF-8 (uint32 len + datos)
            - bytes      -> I + bytes (uint32 len + datos)
            - Struct     -> I + subframe (uint32 len + to_bytes())
        El orden es el de las anotaciones de tipo de la clase.
        """
        hints = get_type_hints(type(self))
        fmt = "!"
        fixed_values = []
        var_chunks = []  # partes variables (str/bytes/substruct)

        for name, typ in hints.items():
            value = getattr(self, name)

            is_optional = False
            origin = getattr(typ, "__origin__", None)
            args = getattr(typ, "__args__", ())
            if origin in (Optional, Union) and args and type(None) in args:
                is_optional = True
                inner_typ = next((a for a in args if a is not type(None)), None)
                if inner_typ is None:
                    raise TypeError(f"Optional sin tipo interno en campo {name}")
                if value is None:
                    fmt += "?"  # booleano para indicar si el campo está presente
                    fixed_values.append(False)
                    continue
                else:
                    fmt += "?"  # booleano para indicar si el campo está presente
                    fixed_values.append(True)
                    typ = inner_typ  # Usamos el tipo interno para la serialización

            if typ is int:
                # usar entero sin signo de 64 bits para evitar desbordes de sess/ids
                fmt += "Q"
                fixed_values.append(int(value))
            elif typ is bool:
                fmt += "?"
                fixed_values.append(bool(value))
            elif typ is float:
                fmt += "d"
                fixed_values.append(float(value))
            elif typ is str:
                data = value.encode("utf-8")
                fmt += "I"  # longitud
                fixed_values.append(len(data))
                var_chunks.append(data)
            elif typ is bytes:
                data = value
                fmt += "I"  # longitud
                fixed_values.append(len(data))
                var_chunks.append(data)
            elif isinstance(typ, type) and issubclass(typ, Struct):
                # Campo que es otro Struct: lo serializamos recursivamente
                sub_blob = value.to_bytes()
                fmt += "I"  # longitud del substruct
                fixed_values.append(len(sub_blob))
                var_chunks.append(sub_blob)
            else:
                raise TypeError(
                    f"Tipo no soportado en Struct: campo '{name}' de tipo {typ}"
                )

        header = struct.pack(fmt, *fixed_values)
        return header + b"".join(var_chunks)

    @classmethod
    def from_bytes(cls: Type[T], blob: bytes) -> T:
        """
        Reconstruye una instancia de la clase a partir de un frame binario
        generado por to_bytes().
        """
        hints = get_type_hints(cls)
        pos = 0
        kwargs = {}

        for name, typ in hints.items():
            is_optional = False
            origin = getattr(typ, "__origin__", None)
            args = getattr(typ, "__args__", ())
            if origin in (Optional, Union) and args and type(None) in args:
                is_optional = True
                inner_typ = next((a for a in args if a is not type(None)), None)
                if inner_typ is None:
                    raise TypeError(f"Optional sin tipo interno en campo {name}")
                typ = inner_typ

                # Flag de presencia (1 byte)
                if pos + 1 > len(blob):
                    raise ValueError("blob truncado al leer flag optional")
                present = struct.unpack_from("!?", blob, pos)[0]
                pos += struct.calcsize("!?")
                if not present:
                    kwargs[name] = None
                    continue

            if typ is int:
                size = struct.calcsize("!Q")
                if pos + size > len(blob):
                    raise ValueError("blob truncado al leer campo fijo")
                kwargs[name] = struct.unpack_from("!Q", blob, pos)[0]
                pos += size
            elif typ is bool:
                size = struct.calcsize("!?")
                if pos + size > len(blob):
                    raise ValueError("blob truncado al leer campo fijo")
                kwargs[name] = struct.unpack_from("!?", blob, pos)[0]
                pos += size
            elif typ is float:
                size = struct.calcsize("!d")
                if pos + size > len(blob):
                    raise ValueError("blob truncado al leer campo fijo")
                kwargs[name] = struct.unpack_from("!d", blob, pos)[0]
                pos += size
            elif typ is str:
                if pos + 4 > len(blob):
                    raise ValueError("blob truncado al leer longitud de campo variable")
                length = struct.unpack_from("!I", blob, pos)[0]
                pos += struct.calcsize("!I")
                chunk = blob[pos:pos + length]
                pos += length
                if len(chunk) != length:
                    raise ValueError("blob truncado al leer campo variable")
                kwargs[name] = chunk.decode("utf-8")
            elif typ is bytes:
                if pos + 4 > len(blob):
                    raise ValueError("blob truncado al leer longitud de campo variable")
                length = struct.unpack_from("!I", blob, pos)[0]
                pos += struct.calcsize("!I")
                chunk = blob[pos:pos + length]
                pos += length
                if len(chunk) != length:
                    raise ValueError("blob truncado al leer campo variable")
                kwargs[name] = chunk
            elif isinstance(typ, type) and issubclass(typ, Struct):
                if pos + 4 > len(blob):
                    raise ValueError("blob truncado al leer longitud de campo variable")
                length = struct.unpack_from("!I", blob, pos)[0]
                pos += struct.calcsize("!I")
                chunk = blob[pos:pos + length]
                pos += length
                if len(chunk) != length:
                    raise ValueError("blob truncado al leer campo variable")
                kwargs[name] = typ.from_bytes(chunk)
            else:
                raise TypeError(
                    f"Tipo no soportado en Struct: campo '{name}' de tipo {typ}"
                )

        return cls(**kwargs)
