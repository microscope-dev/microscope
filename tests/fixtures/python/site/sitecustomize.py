"""A program's own sitecustomize, which microscope's launch bootstrap must
still import."""
import builtins

builtins.MICROSCOPE_TEST_SITE = 1
