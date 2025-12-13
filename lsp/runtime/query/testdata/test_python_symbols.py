#!/usr/bin/env python3
"""Test Python file for symbol extraction tests."""

def hello_world():
    """A simple function."""
    return "Hello, World!"

class MyHephClass:
    """A test class for Heph."""
    
    def __init__(self, name):
        self.name = name
    
    def my_method(self, param1, param2):
        """A method with parameters."""
        result = param1 + param2
        return result
    
    @staticmethod
    def static_method():
        """A static method."""
        return "static"

def another_function(x, y):
    """Another function with calculations."""
    z = x * y
    my_var = 42
    return z + my_var

string_literal = "//heph/string/literal"

# Some variable assignments
my_variable = "test"
another_var = 123
calculation = another_function(3, 4)

# Nested function call
result = MyHephClass("test").my_method(1, 2)