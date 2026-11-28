package ring

type A struct {
	b *B
}

type B struct {
	c *C
}

type C struct {
	d *D
}

type D struct {
	e *E
}

type E struct {
	f *F
}

type F struct {
	g *G
}

type G struct {
	h *H
}

type H struct {
	a *A
}
