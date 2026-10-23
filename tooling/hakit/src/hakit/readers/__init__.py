"""Readers of a hakit run directory and of the series around it (ADR-0049 §6.7).

Each reader is a `hakit` subcommand; what is product-specific in it (pod
selectors, containers, bounds, series lists) is a CLI flag or a `--spec` YAML
the product keeps in its own repository.
"""
