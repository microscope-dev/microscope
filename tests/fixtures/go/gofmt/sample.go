

package   sample

import "fmt"

type Point struct{ X,Y int }

var names = map[string]int{"a":1,"b":2}

func Show(p Point) string {
return fmt.Sprintf("%d,%d",p.X,p.Y)
}
