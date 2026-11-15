




package main

import "C"


var release = make(chan struct{})
var started = make(chan struct{})

func init() {
	go waiting()
	<-started
}


func waiting() {
	close(started) // HOSTED: began
	<-release      // HOSTED: waiting
}



//export Triple
func Triple(value C.long) C.long { // HOSTED: entered
	tripled := value * 3
	close(release)
	return tripled
}

func main() {}
