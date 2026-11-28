from .a import Alpha

class Beta:
    def __init__(self) -> None:
        self.n: int = 0
        self.peer: Alpha | None = None

    def ping(self) -> None:
        self.n += 1

    def run(self, other: Alpha) -> None:
        self.n += 1
        other.ping()
