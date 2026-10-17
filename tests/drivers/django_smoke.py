"""Django's ORM against Chronos: a model made by the schema editor, saved, filtered, aggregated, in a transaction."""
import os
from urllib.parse import urlparse

import django
from django.conf import settings

u = urlparse(os.environ.get("CHRONOS_URL", "postgres://tester@127.0.0.1:5433/main"))
settings.configure(
    DATABASES={
        "default": {
            "ENGINE": "django.db.backends.postgresql",
            "NAME": u.path.lstrip("/"),
            "USER": u.username or "tester",
            "PASSWORD": u.password or "",
            "HOST": u.hostname,
            "PORT": u.port,
        }
    },
    INSTALLED_APPS=[],
    USE_TZ=True,
    DEFAULT_AUTO_FIELD="django.db.models.BigAutoField",
)
django.setup()

from django.db import IntegrityError, connection, models, transaction  # noqa: E402
from django.db.models import Count, Sum  # noqa: E402


class Item(models.Model):
    name = models.CharField(max_length=40, unique=True)
    qty = models.IntegerField(default=0)
    price = models.DecimalField(max_digits=8, decimal_places=2, null=True)
    made = models.DateTimeField(auto_now_add=True)
    meta = models.JSONField(default=dict)

    class Meta:
        app_label = "drv"
        db_table = "drv_django_item"


with connection.cursor() as c:
    c.execute("drop table if exists drv_django_item")
with connection.schema_editor() as editor:
    editor.create_model(Item)

Item.objects.create(name="bolt", qty=3, price="1.25", meta={"k": [1]})
Item.objects.bulk_create([Item(name="nut", qty=10), Item(name="gear", qty=1, price="9.00")])
assert list(Item.objects.filter(qty__gte=3).order_by("name").values_list("name", flat=True)) == ["bolt", "nut"]
assert Item.objects.aggregate(n=Count("id"), q=Sum("qty")) == {"n": 3, "q": 14}
assert Item.objects.get(name="bolt").meta == {"k": [1]}
assert Item.objects.filter(name="nut").update(qty=11) == 1
try:
    with transaction.atomic():
        Item.objects.create(name="washer")
        Item.objects.create(name="bolt")
    raise SystemExit("a duplicate name was stored")
except IntegrityError:
    pass
assert not Item.objects.filter(name="washer").exists(), "the transaction rolled back whole"
assert Item.objects.filter(name="gear").delete()[0] == 1
assert Item.objects.count() == 2
with connection.schema_editor() as editor:
    editor.delete_model(Item)
print("ok")
