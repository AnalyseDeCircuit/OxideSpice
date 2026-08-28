"""Expose TOML parsing on every Python version used by release runners."""

try:
    import tomllib
except ModuleNotFoundError:
    import tomli as tomllib

__all__ = ["tomllib"]
