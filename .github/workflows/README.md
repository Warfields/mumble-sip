
### GitHub CI Docker publish

GitHub Actions publishes `docker/Dockerfile` to Docker Hub as `swarfield/mumble-sip` on pushes to `main` and `v*` tags.

Set these repository secrets before enabling publish:

- `DOCKERHUB_USERNAME`
- `DOCKERHUB_TOKEN`