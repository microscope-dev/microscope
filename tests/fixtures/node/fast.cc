// A Node addon whose function V8 may call as a fast API call, straight from
// optimized code with no exit frame, that blocks in read(2). V8's C++ API
// is not ABI-stable, so it is built once per Node build, against the
// build's headers and the fast API header of its V8.
//
// read(fd): reads one byte from fd and returns read's result.
// nap(microseconds): sleeps that long in nanosleep(2).
// fastCalls(): how many calls of either took the fast path.

#include <node.h>
#include <unistd.h>
#include <v8-fast-api-calls.h>

namespace {

int fast_calls = 0;

int32_t FastRead(v8::Local<v8::Value> /* receiver */, int32_t fd) {
  fast_calls++;
  char byte;
  return static_cast<int32_t>(read(fd, &byte, 1));
}

void SlowRead(const v8::FunctionCallbackInfo<v8::Value>& info) {
  int32_t fd = info[0].As<v8::Int32>()->Value();
  char byte;
  info.GetReturnValue().Set(static_cast<int32_t>(read(fd, &byte, 1)));
}

void FastNap(v8::Local<v8::Value> /* receiver */, int32_t microseconds) {
  fast_calls++;
  usleep(static_cast<useconds_t>(microseconds));
}

void SlowNap(const v8::FunctionCallbackInfo<v8::Value>& info) {
  usleep(static_cast<useconds_t>(info[0].As<v8::Int32>()->Value()));
}

void FastCalls(const v8::FunctionCallbackInfo<v8::Value>& info) {
  info.GetReturnValue().Set(fast_calls);
}

const v8::CFunction fast_read = v8::CFunction::Make(FastRead);
const v8::CFunction fast_nap = v8::CFunction::Make(FastNap);

}  // namespace

NODE_MODULE_INIT() {
  v8::Isolate* isolate = v8::Isolate::GetCurrent();
  auto set = [&](const char* name, v8::Local<v8::FunctionTemplate> function) {
    exports
        ->Set(context, v8::String::NewFromUtf8(isolate, name).ToLocalChecked(),
              function->GetFunction(context).ToLocalChecked())
        .Check();
  };
  set("read", v8::FunctionTemplate::New(isolate, SlowRead, v8::Local<v8::Value>(),
                                        v8::Local<v8::Signature>(), 1,
                                        v8::ConstructorBehavior::kThrow,
                                        v8::SideEffectType::kHasSideEffect, &fast_read));
  set("nap", v8::FunctionTemplate::New(isolate, SlowNap, v8::Local<v8::Value>(),
                                       v8::Local<v8::Signature>(), 1,
                                       v8::ConstructorBehavior::kThrow,
                                       v8::SideEffectType::kHasSideEffect, &fast_nap));
  set("fastCalls", v8::FunctionTemplate::New(isolate, FastCalls));
}
