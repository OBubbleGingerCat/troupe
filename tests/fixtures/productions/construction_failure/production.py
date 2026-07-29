from troupe import Production as BaseProduction


class ConstructBoom(Exception):
    pass


class Production(BaseProduction):
    def __init__(self, args: list[str]) -> None:
        raise ConstructBoom("construct marker")
