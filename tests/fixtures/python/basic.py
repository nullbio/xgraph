import os
from typing import List
from .helpers import foo
from ..pkg import *

MAX_RETRIES = 5

@app.route('/')
def index():
    return 'hello'

class User(BaseUser):
    def __init__(self, name):
        self.name = name

    async def fetch(self, client):
        return await client.session.get(self.url).result()

def chained_call_demo(a):
    return a.b.c()
