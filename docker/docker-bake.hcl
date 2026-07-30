# Build multi-arch de las 5 imágenes y push a Docker Hub.
#   cd <raíz del repo>
#   IMAGE_PREFIX=tuusuario docker buildx bake -f docker/docker-bake.hcl --push
# Para probar en local una sola arch (sin push):
#   docker buildx bake -f docker/docker-bake.hcl --set *.platform=linux/amd64 --load qkc

variable "IMAGE_PREFIX" { default = "ORG" }     # namespace Docker Hub (placeholder)
variable "TAG"          { default = "latest" }

group "default" {
  targets = ["qkc", "orr", "dkms", "sdn", "quditto"]
}

target "_common" {
  context    = "."                    # raíz del repo
  dockerfile = "docker/Dockerfile"
  platforms  = ["linux/amd64", "linux/arm64"]
}

target "qkc" {
  inherits = ["_common"]
  args     = { MODULE = "qkc" }
  tags     = ["${IMAGE_PREFIX}/qkc:${TAG}"]
}

target "orr" {
  inherits = ["_common"]
  args     = { MODULE = "orr" }
  tags     = ["${IMAGE_PREFIX}/orr:${TAG}"]
}

target "dkms" {
  inherits = ["_common"]
  args     = { MODULE = "dkms" }
  tags     = ["${IMAGE_PREFIX}/dkms:${TAG}"]
}

target "sdn" {
  inherits = ["_common"]
  args     = { MODULE = "sdn" }
  tags     = ["${IMAGE_PREFIX}/sdn:${TAG}"]
}

# Simulador de enlace QKD: sirve ETSI-014 con claves aleatorias generadas a
# R(d) = R₀·10^(−α·d/10). Solo hace falta si no tienes hardware QKD.
target "quditto" {
  inherits = ["_common"]
  args     = { MODULE = "quditto" }
  tags     = ["${IMAGE_PREFIX}/quditto:${TAG}"]
}
