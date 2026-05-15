from pydantic import BaseModel


class Model(BaseModel):
    """
    Mixin genérico:
    - Si el nuevo valor es None → se ignora.
    - Si el valor es otro BaseModel y el viejo también → llama a .merge() recursivamente.
    - Si el valor es otro BaseModel y el viejo es None → lo asigna tal cual.
    - Si el valor es primitivo (int, str, etc.) → se sustituye directamente.
    """

    def merge(self, new_model: "Model") -> "Model":
        if new_model is None:
            return self

        for field_name in self.__fields__:
            value_self = getattr(self, field_name, None)
            value_new = getattr(new_model, field_name, None)

            # No pisamos con None
            if value_new is None:
                continue

            # Si el valor nuevo es otro modelo Pydantic
            if isinstance(value_new, BaseModel):
                if value_self is None:
                    # Antes no había nada → pon el nuevo modelo
                    setattr(self, field_name, value_new)
                elif hasattr(value_self, "merge"):
                    # Ambos son modelos y el viejo sabe hacer merge → merge recursivo
                    setattr(self, field_name, value_self.merge(value_new))
                else:
                    # Por si acaso, si no tiene merge, sustituimos
                    setattr(self, field_name, value_new)
            elif isinstance(value_new, list):
                # Para listas intentamos mergear elemento a elemento si son modelos
                if value_new and all(isinstance(item, BaseModel) for item in value_new):
                    existing_by_id = {}
                    if isinstance(value_self, list):
                        existing_by_id = {
                            getattr(item, "id", None): item
                            for item in value_self
                            if isinstance(item, BaseModel) and getattr(item, "id", None) is not None
                        }

                    merged_list = []
                    for item_new in value_new:
                        item_id = getattr(item_new, "id", None)
                        current = existing_by_id.get(item_id) if item_id is not None else None
                        if current and hasattr(current, "merge"):
                            merged_list.append(current.merge(item_new))
                        else:
                            merged_list.append(item_new)
                    setattr(self, field_name, merged_list)
                else:
                    setattr(self, field_name, value_new)
            else:
                # int, str, bool, float, etc. → se sustituyen directamente
                setattr(self, field_name, value_new)

        return self
