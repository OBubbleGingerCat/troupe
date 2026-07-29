class ImportBoom(Exception):
    pass


def fail_during_import() -> None:
    raise ImportBoom("import marker")


fail_during_import()
