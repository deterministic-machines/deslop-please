from .b import Beta

class Alpha:
    def __init__(self) -> None:
        self.n: int = 0
        self.peer: Beta | None = None

    def ping(self) -> None:
        self.n += 1

    def run(self, other: Beta) -> None:
        self.n += 1
        other.ping()
