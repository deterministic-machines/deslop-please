class RunQueue:
    def __init__(self, limit: int) -> None:
        self.items: list[int] = []
        self._limit: int = limit

    def full(self) -> bool:
        return len(self.items) >= self._limit

    @property
    def contents(self) -> list[int]:
        return self.items
