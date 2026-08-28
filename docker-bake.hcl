variable "REGISTRY" {
  default = "zondax"
}

variable "IMAGE_TAG" {
  default = "dev"
}

variable "VERSION" {
  default = "0.0.0"
}

variable "BUILD_VERSION" {
  default = "dev"
}

variable "BUILD_COMMIT" {
  default = "unknown"
}

variable "BUILD_DATE" {
  default = "unknown"
}

variable "PLATFORM" {
  default = "linux/amd64"
}

function "tags" {
  params = [name]
  result = compact([
    notequal(IMAGE_TAG, "") ? "${REGISTRY}/${name}:${IMAGE_TAG}" : "",
    notequal(BUILD_COMMIT, "unknown") ? "${REGISTRY}/${name}:sha-${substr(BUILD_COMMIT, 0, 7)}" : "",
    notequal(VERSION, "0.0.0") ? "${REGISTRY}/${name}:latest" : "",
    notequal(VERSION, "0.0.0") ? "${REGISTRY}/${name}:v${VERSION}" : "",
    notequal(VERSION, "0.0.0") ? "${REGISTRY}/${name}:v${split(".", VERSION)[0]}.${split(".", VERSION)[1]}" : "",
    notequal(VERSION, "0.0.0") ? "${REGISTRY}/${name}:v${split(".", VERSION)[0]}" : "",
  ])
}

group "default" {
  targets = ["kartero"]
}

group "push" {
  targets = ["kartero-push"]
}

target "kartero" {
  dockerfile = "docker/kartero.Dockerfile"
  context    = "."
  platforms  = [PLATFORM]
  tags       = tags("kartero")
  args = {
    BUILD_VERSION = BUILD_VERSION
    BUILD_COMMIT  = BUILD_COMMIT
    BUILD_DATE    = BUILD_DATE
  }
}

target "kartero-push" {
  inherits = ["kartero"]
  output   = ["type=registry"]
  attest   = ["type=provenance,mode=max", "type=sbom"]
}
