from troupe import Production


def missing_argument() -> Production:
    return Production()


def too_many_arguments() -> Production:
    return Production([], [])


def keyword_argument() -> Production:
    return Production(args=[])


def non_list_argument() -> Production:
    return Production(())


def non_string_item() -> Production:
    return Production([1])


def missing_args_attribute(production: Production) -> object:
    return production.args


class WrongHookReturn(Production):
    async def start(self) -> int:
        return 1
