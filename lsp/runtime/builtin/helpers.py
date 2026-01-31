from typing import Union, List, Optional, Literal, Any, Dict


def text_file(
    name: str,
    out: str,
    text: str,
):
    """Creates a target which outputs a text file

    Args:
        name (str): Target name (required)
        out (str): Output file path
        text (str): Text content to write to the file
    """
    pass


def json_file(
    name: str,
    out: str,
    data: Dict[str, Any],
):
    """Creates a target which outputs a json file from a Starlark object

    Args:
        name (str): Target name (required)
        out (str): Output file path
        data (Dict[str, Any]): JSON data to write to the file
    """
    pass


def tool_target(
    name: str,
    tools: Union[str, List[str], dict],
):
    """Creates a target which will proxy invocation to a binary

    This is useful to allow execution of binaries from the cli

    Args:
        name (str): Target name (required)
        tools (Union[str, List[str], dict]): Tools to be exposed to this target
            (available in `PATH`)
    """
    pass


def group(
    name: str,
    deps: Union[str, List[str], dict],
):
    """Creates a target which only collects & outputs a set of files

    Args:
        name (str): Target name (required)
        deps (Union[str, List[str], dict]): Dependencies required by this target
            (target and files), will be copied to the sandbox and part of the hash
    """
    pass


def load(name: str, *funs: str):
    """
    Loads Heph script from path. Need to load at least one function.
    Args:
        name (str): Heph BUILD path without file
        funs (str): List of functions loaded
    """
    pass
