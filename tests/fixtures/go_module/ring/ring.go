package ring

type A struct{ Next *B }
type B struct{ Next *C }
type C struct{ Next *D }
type D struct{ Next *E }
type E struct{ Next *F }
type F struct{ Next *G }
type G struct{ Next *H }
type H struct{ Next *A }
