# Ops

## Tagging a release

Tag current commit (annotated) so `.github/workflows/version.sh` sees it:

```bash
git tag -a v1.0.0 -m "v1.0.0"
git push origin v1.0.0
```
