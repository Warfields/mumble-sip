
### GitHub CI Docker publish

GitHub Actions publishes Docker images to Docker Hub on pushes to `main` and `v*` tags:

- `docker/Dockerfile` as `swarfield/mumble-sip`
- `docker/Dockerfile.pocket-tts` as `swarfield/pocket-tts`

Set these repository secrets before enabling publish:

- `DOCKERHUB_USERNAME`
- `DOCKERHUB_TOKEN`
