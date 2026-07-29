from troupe import Production as BaseProduction


class Production(BaseProduction):
    def __init__(self, args: list[str]) -> None:
        self.received = args
