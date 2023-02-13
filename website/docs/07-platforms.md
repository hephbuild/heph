# Platforms

heph has the capability to run targets locally or remotely.

Platforms are declared in `.hephconfig` under the `platforms` key:

```yaml title=.hephconfig
platforms:
  some_name:
    provider: some_provider
```

> The key specified for each platform will be used as the `name` label (here `name=some_name`)

## Usage

When declaring a target, specify the platform constrains for it:

```python
target(
    name="build",
    platforms={
        "name": "some_name",
        "os": "linux",
        "options": {
            ...
        },
    }
)
```

`options` is a special key that can be used to configure the platform

This configuration will schedule running `//:build` to the platform `some_name` on the OS `linux` 

## Available platforms

### `local`

It is configured by default with the following labels:
```python
{
    "name": "local",
    "os": "<current os>",
    "arch": "<current arch>",
}
```

> `os` and `arch` values are taken from go, see [here](https://github.com/golang/go/blob/master/src/go/build/syslist.go) for available values

### `docker`

```yaml title=.hephconfig
platforms:
  docker:
    provider: docker
```

It is configured with the following labels:
```python
{
    "name": "docker",
    "os": "<os>",
    "arch": "<arch>",
}
```

Reference the platform:

```python title=BUILD
target(
    name="build",
    run="uname -sr",
    platforms={
        "name": "docker",
        "os": "linux",
        "options": {
            "image": "alpine:latest",
        },
    }
)
```
> `os` and `arch` values will be set to available OS/ARCH of your Docker Engine
> 
> Typically on linux, `os` and `arch` will match your host, where on macOS because docker is running in a VM it will be set to `os=linux` and `arch=amd64`
