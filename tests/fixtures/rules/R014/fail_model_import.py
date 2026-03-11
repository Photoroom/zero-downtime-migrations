# R014: RunPython with direct model import - should fail
from django.db import migrations
from myapp.models import Order  # Bad! Should use apps.get_model()


def forwards_func(apps, schema_editor):
    Order.objects.filter(status='pending').update(status='active')


class Migration(migrations.Migration):

    dependencies = [
        ('myapp', '0012'),
    ]

    operations = [
        migrations.RunPython(forwards_func),
    ]
