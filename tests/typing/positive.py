from troupe import Production


class ExampleProduction(Production):
    def __init__(self, args: list[str]) -> None:
        self.received = args

    async def start(self) -> None:
        return None

    async def scene(self) -> None:
        return None

    async def stop(self) -> None:
        return None


async def exercise() -> None:
    production = ExampleProduction(["--value", "1"])
    await production.start()
    await production.scene()
    await production.stop()

    base = Production(["\udcff"])
    await base.start()
    await base.stop()
