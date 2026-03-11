# R012: RunPython without reverse_code - should warn
from django.db import migrations


def forwards_func(apps, schema_editor):
    Order = apps.get_model('myapp', 'Order')
    Order.objects.filter(status='pending').update(status='active')


class Migration(migrations.Migration):

    dependencies = [
        ('myapp', '0009'),
    ]

    operations = [
        migrations.RunPython(forwards_func),
    ]
